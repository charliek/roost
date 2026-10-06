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

    /// The inverse of [`Why::wire`], for a reader of `host.status`.
    pub fn from_wire(wire: &str) -> Option<Self> {
        [
            Self::Missing,
            Self::Unreadable,
            Self::Older,
            Self::Incompatible,
            Self::Override,
        ]
        .into_iter()
        .find(|why| why.wire() == wire)
    }

    pub fn reason(self) -> &'static str {
        match self {
            Self::Missing => "its roost-session is gone",
            Self::Unreadable => "can't read its roost-session",
            Self::Older => "its roost-session is older",
            Self::Incompatible => "its roost-session can't talk to this Roost",
            Self::Override => "ROOST_SESSION_BIN names nothing this user can run",
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

/// D3 rule 1: a refused session is the newer side when it speaks a newer
/// protocol, or when this client's version orders older than its own.
pub fn refused_session_newer(running: &BuildId, client: &BuildId, protocol_newer: bool) -> bool {
    protocol_newer || order(client, running) == VersionOrder::Older
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
        return if refused_session_newer(running, client, protocol_newer) {
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
    /// The newer build an ssh host would exec next, as this client's own
    /// install or a probe found it (D5) — what "installed" names,
    /// whether or not a restart could use it.
    pub staged: Option<BuildId>,
}

impl UpdateFacts {
    pub fn new(inputs: UpdateInputs<'_>) -> Self {
        let state = classify(inputs);
        Self {
            state,
            running: inputs.running.clone(),
            client: inputs.client.clone(),
            restart: restart_offer(state, inputs.transport, inputs.target),
            staged: match inputs.install {
                InstallKnowledge::Staged(build)
                    if order(build, inputs.running) == VersionOrder::Newer =>
                {
                    Some(build.clone())
                }
                _ => None,
            },
        }
    }

    /// As `host.status` spells it (D7).
    pub fn status(&self) -> HostUpdateStatus {
        HostUpdateStatus {
            state: self.state.wire().to_string(),
            blocked: match self.state {
                SessionUpdate::SessionNewer { blocked } => Some(blocked),
                _ => None,
            },
            session: BuildStatus::from(&self.running),
            client: BuildStatus::from(&self.client),
            staged: self.staged.as_ref().map(BuildStatus::from),
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
            action: None,
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

// ---------------------------------------------------------------------------
// D7: which action is offered, and why not
// ---------------------------------------------------------------------------

/// Why Install Update is not offered (D7). Each is also what the op
/// answers and what a card says instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallRefusal {
    /// Only an ssh host has a binary this client can replace.
    NotSsh,
    /// This client is older than the running session.
    ClientOlder,
    /// The session speaks another protocol, so it needs the update that
    /// also restarts it. Installing alone would leave the tabs' agent
    /// hooks talking a protocol their daemon does not.
    ProtocolDiffers,
    /// The exec rung is already this client's build, or newer.
    AlreadyInstalled,
    /// The state has nothing for an install to fix.
    NotOffered(SessionUpdate),
}

impl InstallRefusal {
    pub fn reason(self) -> &'static str {
        match self {
            Self::NotSsh => "only an ssh host has a roost-session this Roost can install",
            Self::ClientOlder => "the session is newer than this Roost; update this Roost instead",
            Self::ProtocolDiffers => {
                "the session speaks another protocol; it needs Update roost-session, which also \
                 restarts it"
            }
            Self::AlreadyInstalled => "that build is already installed; restart to use it",
            Self::NotOffered(state) => match state {
                SessionUpdate::UpToDate => "the session is already up to date",
                SessionUpdate::Staged => "the update is already installed; restart to use it",
                SessionUpdate::Required => "the session needs Update roost-session",
                SessionUpdate::SessionNewer { .. } => "the session is newer than this Roost",
                SessionUpdate::Unordered => {
                    "this Roost cannot tell whether its build is newer than the session's"
                }
                SessionUpdate::Unknown => "the session did not say which build it runs",
                SessionUpdate::Available => "nothing to install",
            },
        }
    }
}

/// Whether Install Update is offered, before anything has been probed.
pub fn install_offer(
    facts: &UpdateFacts,
    transport: HostTransportKind,
) -> Result<(), InstallRefusal> {
    if transport != HostTransportKind::Ssh {
        return Err(InstallRefusal::NotSsh);
    }
    install_refusal(&facts.running, &facts.client, None)?;
    match facts.state {
        SessionUpdate::Available => Ok(()),
        state => Err(InstallRefusal::NotOffered(state)),
    }
}

/// D7's refusals, including what a probe found at the exec rung.
///
/// A rung that matches this client by version, protocol and build is
/// "already installed" whatever its dev facts say: that is the triple an
/// install is judged by, so writing it again would change nothing.
pub fn install_refusal(
    running: &BuildId,
    client: &BuildId,
    rung: Option<&BuildId>,
) -> Result<(), InstallRefusal> {
    if order(client, running) == VersionOrder::Older {
        return Err(InstallRefusal::ClientOlder);
    }
    if client.protocol != running.protocol {
        return Err(InstallRefusal::ProtocolDiffers);
    }
    if let Some(rung) = rung {
        if rung.same_install(client) || order(rung, client) == VersionOrder::Newer {
            return Err(InstallRefusal::AlreadyInstalled);
        }
    }
    Ok(())
}

/// Whether Update roost-session (install, then restart) is offered: only
/// for a refused ssh session (D6).
pub fn update_offered(facts: &UpdateFacts, transport: HostTransportKind) -> bool {
    transport == HostTransportKind::Ssh && facts.state == SessionUpdate::Required
}

/// Why Restart Session is not offered, or `None` when it is.
pub fn restart_refusal(facts: &UpdateFacts, transport: HostTransportKind) -> Option<&'static str> {
    if facts.restart.offered {
        return None;
    }
    Some(match (facts.restart.why, facts.state, transport) {
        (Some(why), ..) => why.reason(),
        (None, _, HostTransportKind::Socket) => {
            "a socket host's session is not this Roost's to restart"
        }
        (None, SessionUpdate::SessionNewer { .. }, _) => {
            "the session is newer than this Roost; update this Roost to connect"
        }
        (None, SessionUpdate::Required, _) => "the session needs Update roost-session",
        (None, ..) => "nothing to restart onto",
    })
}

/// What the band, the palette and the pill offer for a reduced-fidelity
/// connection under Option 2 (D6): Install when the ssh rung is stale,
/// Restart when a build is staged or the host is this machine, and
/// nothing to press otherwise. Without update facts the transport's
/// own answer stands.
pub fn fidelity_route(
    action: Option<crate::host_sidebar::FidelityAction>,
    facts: Option<&UpdateFacts>,
    transport: HostTransportKind,
) -> Option<crate::host_sidebar::FidelityAction> {
    use crate::host_sidebar::FidelityAction;
    let (Some(FidelityAction::Update | FidelityAction::Restart), Some(facts)) = (action, facts)
    else {
        return action;
    };
    if install_offer(facts, transport).is_ok() {
        return Some(FidelityAction::Update);
    }
    // A session newer than this client, or of a build it cannot order,
    // is not one a restart onto "the newest build" would bring level.
    let restartable = !matches!(
        facts.state,
        SessionUpdate::SessionNewer { .. } | SessionUpdate::Unordered
    ) && (facts.state == SessionUpdate::Staged
        || transport == HostTransportKind::Localhost);
    Some(if facts.restart.offered && restartable {
        FidelityAction::Restart
    } else {
        FidelityAction::Manual
    })
}

/// A build as a person reads it: `0.0.22`, or `0.0.22 dev a1b2c3d`.
pub fn describe(build: &BuildId) -> String {
    describe_parts(&build.version, build.dev, build.sha.as_deref())
}

/// [`describe`], from the fields `host.status` carries.
pub fn describe_parts(version: &str, dev: bool, sha: Option<&str>) -> String {
    match dev_marker(dev, sha) {
        Some(marker) => format!("{version} {marker}"),
        None => version.to_string(),
    }
}

/// `dev a1b2c3d`, or `dev` alone without a sha; `None` for a release.
fn dev_marker(dev: bool, sha: Option<&str>) -> Option<String> {
    match (dev, sha) {
        (false, _) => None,
        (true, Some(sha)) => Some(format!("dev {sha}")),
        (true, None) => Some("dev".to_string()),
    }
}

/// The line a host's menu opens with (D6's band-menu table): the
/// session's build and what can be done about it, then why Restart is
/// absent when a candidate was judged unusable.
pub fn session_line(facts: &UpdateFacts, transport: HostTransportKind) -> String {
    let running = &facts.running;
    let session = if running.version.is_empty() {
        "Session version unknown".to_string()
    } else {
        format!("Session {}", describe(running))
    };
    let available = |offered: &BuildId| {
        // "Matching" only when it is this client's own build, every
        // fidelity-relevant field of it, and the version reads the same
        // as the running one's.
        if offered == &facts.client && describe(offered) == describe(running) {
            format!("{session} · a matching build available")
        } else {
            format!("{session} · {} available", describe(offered))
        }
    };
    // Nothing is known to be installed or found, only that a restart is
    // the fix — and with no usable target, not even that: the reason
    // suffix says why.
    let restart_to_match = || {
        if facts.restart.offered {
            format!("{session} · restart to use a matching build")
        } else {
            session.clone()
        }
    };
    let line = match (facts.state, transport) {
        (_, HostTransportKind::Socket) => session.clone(),
        (SessionUpdate::UpToDate, _) => format!("{session} · up to date"),
        (SessionUpdate::Available, _) => available(&facts.client),
        (SessionUpdate::Staged, HostTransportKind::Ssh) => match &facts.staged {
            Some(staged) if facts.restart.offered => {
                format!("{} installed · restart to use it", describe(staged))
            }
            Some(staged) => format!("{} installed", describe(staged)),
            None => restart_to_match(),
        },
        (SessionUpdate::Staged, _) => match &facts.restart.target {
            Some(target) => available(&target.identity),
            None => restart_to_match(),
        },
        (SessionUpdate::SessionNewer { blocked: false }, _) => {
            format!("{session} · newer than this Roost")
        }
        (SessionUpdate::SessionNewer { blocked: true }, _) => {
            format!("{session} · update this Roost to connect")
        }
        (SessionUpdate::Unordered, _) => {
            match dev_marker(facts.client.dev, facts.client.sha.as_deref()) {
                Some(marker) => format!("{session} · this Roost {marker}"),
                None => session.clone(),
            }
        }
        (SessionUpdate::Required, _) => {
            format!("{session} · {} needed to connect", describe(&facts.client))
        }
        (SessionUpdate::Unknown, _) => session.clone(),
    };
    match facts.restart.why {
        Some(why) => format!("{line} · {}", why.reason()),
        None => line,
    }
}

/// Why a reduced-fidelity band offers nothing to press — what its row
/// says in place of an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FidelityNote {
    /// Somebody else's process: whoever runs it restarts it.
    Socket,
    /// The session is newer than this client; this Roost is what needs
    /// updating.
    UpdateRoost,
    /// Neither build orders against the other.
    Unordered,
    /// No candidate a restart could run is usable.
    Unusable(Why),
    /// Nothing more specific is known.
    NoMatchingBuild,
}

/// [`FidelityNote`] for a band whose fidelity pill offers nothing.
pub fn fidelity_note(facts: Option<&UpdateFacts>, transport: HostTransportKind) -> FidelityNote {
    if transport == HostTransportKind::Socket {
        return FidelityNote::Socket;
    }
    let Some(facts) = facts else {
        return FidelityNote::NoMatchingBuild;
    };
    match (facts.state, facts.restart.why) {
        (SessionUpdate::SessionNewer { .. }, _) => FidelityNote::UpdateRoost,
        (SessionUpdate::Unordered, _) => FidelityNote::Unordered,
        (_, Some(why)) => FidelityNote::Unusable(why),
        _ => FidelityNote::NoMatchingBuild,
    }
}

/// A project's or tab's host block header (D6): the host, and its
/// session line once one is known.
pub fn host_header(label: &str, session_line: Option<&str>) -> String {
    match session_line {
        Some(line) => format!("{label} · {line}"),
        None => label.to_string(),
    }
}

/// The band's must-act pill (D6): the session refused this client and
/// one side has to change. Nothing else on the band names a version.
pub fn band_pill(facts: Option<&UpdateFacts>) -> Option<&'static str> {
    match facts?.state {
        SessionUpdate::Required => Some("needs update"),
        SessionUpdate::SessionNewer { blocked: true } => Some("update Roost"),
        _ => None,
    }
}

/// D8's check after a restart: a different session, running exactly
/// the build the restart was aimed at. `Err` carries what is serving,
/// for "restarted, but the session is still …".
pub fn verify_restart(
    before: &str,
    target: &BuildId,
    after_id: &str,
    after: &BuildId,
) -> Result<(), String> {
    if after_id != before && after == target {
        return Ok(());
    }
    if after_id == before {
        return Err(format!("{} (the same session)", describe(after)));
    }
    Err(describe(after))
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
        assert_eq!(status.staged, None, "nothing was installed on localhost");
        assert!(status.restart.offered);
        assert_eq!(
            status.restart.target.map(|t| (t.version, t.source)),
            Some(("0.0.22".to_string(), "bundled".to_string()))
        );
    }

    fn facts(state: SessionUpdate, running: &str, client: &str) -> UpdateFacts {
        UpdateFacts {
            state,
            running: release(running),
            client: release(client),
            restart: RestartOffer {
                offered: true,
                why: None,
                target: None,
            },
            staged: None,
        }
    }

    #[test]
    fn install_is_offered_only_over_ssh_for_an_available_build() {
        let available = facts(SessionUpdate::Available, "0.0.21", "0.0.22");
        assert_eq!(install_offer(&available, Ssh), Ok(()));
        assert_eq!(
            install_offer(&available, Localhost),
            Err(InstallRefusal::NotSsh)
        );
        assert_eq!(
            install_offer(&available, Socket),
            Err(InstallRefusal::NotSsh)
        );
        for state in [
            SessionUpdate::UpToDate,
            SessionUpdate::Staged,
            SessionUpdate::Required,
            SessionUpdate::Unordered,
            SessionUpdate::Unknown,
        ] {
            let other = UpdateFacts {
                state,
                ..available.clone()
            };
            assert_eq!(
                install_offer(&other, Ssh),
                Err(InstallRefusal::NotOffered(state))
            );
        }
    }

    #[test]
    fn install_is_refused_by_each_of_d7s_three_rules() {
        let running = release("0.0.21");
        let client = release("0.0.22");
        assert_eq!(install_refusal(&running, &client, None), Ok(()));
        // This client is older than the session.
        assert_eq!(
            install_refusal(&release("0.0.23"), &client, None),
            Err(InstallRefusal::ClientOlder)
        );
        // The protocols differ.
        let other_protocol = BuildId {
            protocol: PROTOCOL + 1,
            ..release("0.0.21")
        };
        assert_eq!(
            install_refusal(&other_protocol, &client, None),
            Err(InstallRefusal::ProtocolDiffers)
        );
        // The rung is newer than this client, or already it.
        for rung in [release("0.0.23"), release("0.0.22"), dev("0.0.22", "b")] {
            assert_eq!(
                install_refusal(&running, &client, Some(&rung)),
                Err(InstallRefusal::AlreadyInstalled),
                "{rung:?}"
            );
        }
        // A stale rung is what an install replaces.
        assert_eq!(
            install_refusal(&running, &client, Some(&release("0.0.20"))),
            Ok(())
        );
        // An older client is refused ahead of anything a probe found.
        let available = facts(SessionUpdate::Available, "0.0.23", "0.0.22");
        assert_eq!(
            install_offer(&available, Ssh),
            Err(InstallRefusal::ClientOlder)
        );
    }

    #[test]
    fn update_is_the_refused_ssh_sessions_alone() {
        let required = facts(SessionUpdate::Required, "0.0.19", "0.0.22");
        assert!(update_offered(&required, Ssh));
        assert!(!update_offered(&required, Localhost));
        assert!(!update_offered(
            &facts(SessionUpdate::Available, "0.0.21", "0.0.22"),
            Ssh
        ));
    }

    #[test]
    fn a_restart_refusal_names_its_reason() {
        let mut offered = facts(SessionUpdate::Staged, "0.0.21", "0.0.22");
        assert_eq!(restart_refusal(&offered, Localhost), None);
        offered.restart.offered = false;
        offered.restart.why = Some(Why::Missing);
        assert_eq!(
            restart_refusal(&offered, Localhost),
            Some("its roost-session is gone")
        );
        offered.restart.why = None;
        offered.state = SessionUpdate::SessionNewer { blocked: true };
        assert!(restart_refusal(&offered, Ssh)
            .unwrap()
            .contains("update this Roost"));
        assert!(restart_refusal(&offered, Socket)
            .unwrap()
            .contains("socket host"));
    }

    #[test]
    fn reduced_fidelity_routes_to_option_2() {
        use crate::host_sidebar::FidelityAction::{Manual, Restart, Update};
        let mut ssh = facts(SessionUpdate::Available, "0.0.22", "0.0.22");
        assert_eq!(fidelity_route(Some(Update), Some(&ssh), Ssh), Some(Update));
        ssh.state = SessionUpdate::Staged;
        assert_eq!(fidelity_route(Some(Update), Some(&ssh), Ssh), Some(Restart));
        ssh.state = SessionUpdate::SessionNewer { blocked: false };
        assert_eq!(fidelity_route(Some(Update), Some(&ssh), Ssh), Some(Manual));
        ssh.state = SessionUpdate::Unordered;
        assert_eq!(fidelity_route(Some(Update), Some(&ssh), Ssh), Some(Manual));

        let mut local = facts(SessionUpdate::Staged, "0.0.22", "0.0.22");
        assert_eq!(
            fidelity_route(Some(Restart), Some(&local), Localhost),
            Some(Restart)
        );
        local.restart.offered = false;
        assert_eq!(
            fidelity_route(Some(Restart), Some(&local), Localhost),
            Some(Manual)
        );
        // Review finding 4: a localhost session newer than this client,
        // or unordered against it, has a usable target (its own binary)
        // and still nothing a restart would bring level.
        local.restart.offered = true;
        for state in [
            SessionUpdate::SessionNewer { blocked: false },
            SessionUpdate::Unordered,
        ] {
            local.state = state;
            assert_eq!(
                fidelity_route(Some(Restart), Some(&local), Localhost),
                Some(Manual),
                "{state:?}"
            );
        }
        // No facts yet: the transport's own answer.
        assert_eq!(fidelity_route(Some(Update), None, Ssh), Some(Update));
        assert_eq!(fidelity_route(None, Some(&local), Localhost), None);
        assert_eq!(
            fidelity_route(Some(Manual), Some(&local), Socket),
            Some(Manual)
        );
    }

    /// D6's band-menu table, one row per state.
    #[test]
    fn the_session_line_follows_the_band_menu_table() {
        let with = |state, running: BuildId, client: BuildId| UpdateFacts {
            state,
            running,
            client,
            restart: RestartOffer {
                offered: true,
                why: None,
                target: None,
            },
            staged: None,
        };
        let targeted = |mut facts: UpdateFacts, identity: BuildId| {
            facts.restart.target = Some(RestartTarget {
                path: "/x".into(),
                identity,
                source: TargetSource::Bundled,
                session_id: "s".into(),
                generation: 1,
            });
            facts
        };
        let r = release;
        let cases = [
            (
                with(SessionUpdate::UpToDate, r("0.0.22"), r("0.0.22")),
                Ssh,
                "Session 0.0.22 · up to date",
            ),
            (
                with(SessionUpdate::Available, r("0.0.21"), r("0.0.22")),
                Ssh,
                "Session 0.0.21 · 0.0.22 available",
            ),
            // Reduced fidelity at an equal version: the build, not the
            // version, is what is on offer.
            (
                with(SessionUpdate::Available, r("0.0.22"), r("0.0.22")),
                Ssh,
                "Session 0.0.22 · a matching build available",
            ),
            (
                UpdateFacts {
                    staged: Some(r("0.0.22")),
                    ..with(SessionUpdate::Staged, r("0.0.21"), r("0.0.22"))
                },
                Ssh,
                "0.0.22 installed · restart to use it",
            ),
            // Localhost's staged reads "available": nothing was
            // installed, the bundled build is simply newer.
            (
                targeted(
                    with(SessionUpdate::Staged, r("0.0.21"), r("0.0.21")),
                    r("0.0.22"),
                ),
                Localhost,
                "Session 0.0.21 · 0.0.22 available",
            ),
            (
                with(
                    SessionUpdate::SessionNewer { blocked: false },
                    r("0.0.23"),
                    r("0.0.22"),
                ),
                Localhost,
                "Session 0.0.23 · newer than this Roost",
            ),
            (
                with(
                    SessionUpdate::Unordered,
                    dev("0.0.22", "a1b2c3d"),
                    r("0.0.22"),
                ),
                Ssh,
                "Session 0.0.22 dev a1b2c3d",
            ),
            (
                with(
                    SessionUpdate::Unordered,
                    dev("0.0.22", "a1b2c3d"),
                    dev("0.0.22", "f00ba12"),
                ),
                Ssh,
                "Session 0.0.22 dev a1b2c3d · this Roost dev f00ba12",
            ),
            (
                with(
                    SessionUpdate::Unordered,
                    r("0.0.22"),
                    BuildId {
                        dev: true,
                        ..r("0.0.22")
                    },
                ),
                Localhost,
                "Session 0.0.22 · this Roost dev",
            ),
            (
                with(SessionUpdate::Required, r("0.0.19"), r("0.0.22")),
                Ssh,
                "Session 0.0.19 · 0.0.22 needed to connect",
            ),
            (
                with(
                    SessionUpdate::SessionNewer { blocked: true },
                    r("0.0.23"),
                    r("0.0.22"),
                ),
                Ssh,
                "Session 0.0.23 · update this Roost to connect",
            ),
            // A socket host's session is somebody else's: the build, and
            // nothing to do about it.
            (
                with(SessionUpdate::UpToDate, r("0.0.21"), r("0.0.22")),
                Socket,
                "Session 0.0.21",
            ),
            (
                with(SessionUpdate::Unknown, r(""), r("0.0.22")),
                Ssh,
                "Session version unknown",
            ),
        ];
        for (facts, transport, want) in cases {
            assert_eq!(session_line(&facts, transport), want, "{:?}", facts.state);
        }
    }

    /// Review finding 2: "installed" names the staged build itself, not
    /// this client's, even when a restart cannot use it.
    #[test]
    fn the_ssh_staged_line_names_the_build_that_is_installed() {
        let running = release("0.0.21");
        let client = release("0.0.22");
        let rung = BuildId {
            protocol: PROTOCOL + 1,
            ..release("0.0.23")
        };
        let (target, install) = ssh_knowledge(
            &ProbeOutcome::Mismatch {
                path: "/usr/bin/roost-session".into(),
                identity: Some(roost_ipc::messages::SessionBinaryIdentity {
                    app_version: "0.0.23".into(),
                    session_protocol: PROTOCOL + 1,
                    libghostty_build: "g".into(),
                    ..Default::default()
                }),
            },
            &running,
            &client,
            "s",
            1,
        );
        assert_eq!(install, InstallKnowledge::Staged(rung));
        let facts = UpdateFacts::new(UpdateInputs {
            running: &running,
            client: &client,
            gate: Gate::Ok,
            target: &target,
            install: &install,
            transport: Ssh,
        });
        assert_eq!(facts.state, SessionUpdate::Staged);
        assert_eq!(
            session_line(&facts, Ssh),
            "0.0.23 installed · its roost-session can't talk to this Roost"
        );
        let status = facts.status();
        assert_eq!(status.staged.map(|b| b.version), Some("0.0.23".to_string()));
        assert!(!status.restart.offered);
        // This client's own install stages its own build.
        let own = UpdateFacts::new(UpdateInputs {
            running: &running,
            client: &client,
            gate: Gate::Ok,
            target: &TargetKnowledge::NotChecked,
            install: &InstallKnowledge::Staged(client.clone()),
            transport: Ssh,
        });
        assert_eq!(
            session_line(&own, Ssh),
            "0.0.22 installed · restart to use it"
        );
    }

    /// Review finding 1: a session with no version still says what state
    /// it is in and why Restart is absent.
    #[test]
    fn an_unknown_version_keeps_the_state_and_the_reason() {
        let unknown = |state| facts(state, "", "0.0.22");
        assert_eq!(
            session_line(&unknown(SessionUpdate::Required), Ssh),
            "Session version unknown · 0.0.22 needed to connect"
        );
        assert_eq!(
            session_line(&unknown(SessionUpdate::SessionNewer { blocked: true }), Ssh),
            "Session version unknown · update this Roost to connect"
        );
        assert_eq!(
            session_line(&unknown(SessionUpdate::Available), Ssh),
            "Session version unknown · 0.0.22 available"
        );
        let mut staged = unknown(SessionUpdate::Staged);
        staged.restart.target = Some(RestartTarget {
            path: "/x".into(),
            identity: release("0.0.22"),
            source: TargetSource::Bundled,
            session_id: "s".into(),
            generation: 1,
        });
        assert_eq!(
            session_line(&staged, Localhost),
            "Session version unknown · 0.0.22 available"
        );
        let mut unreadable = unknown(SessionUpdate::Unknown);
        unreadable.restart.offered = false;
        unreadable.restart.why = Some(Why::Unreadable);
        assert_eq!(
            session_line(&unreadable, Localhost),
            "Session version unknown · can't read its roost-session"
        );
    }

    /// A reduced-fidelity localhost session is staged before its target
    /// is resolved: "restart to use a matching build" while that is
    /// pending, and only the reason once no candidate is usable.
    #[test]
    fn a_staged_session_with_no_target_says_restart_only_while_one_may_exist() {
        let mut facts = facts(SessionUpdate::Staged, "0.0.22", "0.0.22");
        assert_eq!(
            session_line(&facts, Localhost),
            "Session 0.0.22 · restart to use a matching build"
        );
        facts.restart.offered = false;
        facts.restart.why = Some(Why::Missing);
        assert_eq!(
            session_line(&facts, Localhost),
            "Session 0.0.22 · its roost-session is gone"
        );
    }

    /// Review finding 5: "matching" only for this client's own build. A
    /// target at the same version on another libghostty build is named.
    #[test]
    fn a_matching_build_is_this_clients_whole_identity() {
        let mut facts = facts(SessionUpdate::Staged, "0.0.22", "0.0.22");
        let skewed = BuildId {
            libghostty_build: "other".into(),
            ..release("0.0.22")
        };
        facts.restart.target = Some(RestartTarget {
            path: "/x".into(),
            identity: skewed,
            source: TargetSource::Running,
            session_id: "s".into(),
            generation: 1,
        });
        assert_eq!(
            session_line(&facts, Localhost),
            "Session 0.0.22 · 0.0.22 available"
        );
        facts.restart.target.as_mut().unwrap().identity = release("0.0.22");
        assert_eq!(
            session_line(&facts, Localhost),
            "Session 0.0.22 · a matching build available"
        );
    }

    #[test]
    fn a_blocked_fidelity_pill_says_why() {
        let note = |state, why| {
            let mut facts = facts(state, "0.0.22", "0.0.22");
            facts.restart.why = why;
            fidelity_note(Some(&facts), Ssh)
        };
        assert_eq!(
            fidelity_note(None, Socket),
            FidelityNote::Socket,
            "a socket host, whatever is known"
        );
        assert_eq!(
            note(
                SessionUpdate::SessionNewer { blocked: false },
                Some(Why::Older)
            ),
            FidelityNote::UpdateRoost
        );
        assert_eq!(
            note(SessionUpdate::Unordered, None),
            FidelityNote::Unordered
        );
        assert_eq!(
            note(SessionUpdate::UpToDate, Some(Why::Missing)),
            FidelityNote::Unusable(Why::Missing)
        );
        assert_eq!(
            note(SessionUpdate::Unknown, None),
            FidelityNote::NoMatchingBuild
        );
        assert_eq!(
            fidelity_note(None, Localhost),
            FidelityNote::NoMatchingBuild
        );
    }

    #[test]
    fn an_unusable_restart_says_why_after_the_line() {
        let mut facts = facts(SessionUpdate::UpToDate, "0.0.22", "0.0.22");
        facts.restart.offered = false;
        for (why, suffix) in [
            (Why::Missing, "its roost-session is gone"),
            (Why::Unreadable, "can't read its roost-session"),
            (Why::Older, "its roost-session is older"),
            (
                Why::Incompatible,
                "its roost-session can't talk to this Roost",
            ),
        ] {
            facts.restart.why = Some(why);
            assert_eq!(
                session_line(&facts, Localhost),
                format!("Session 0.0.22 · up to date · {suffix}")
            );
        }
    }

    #[test]
    fn a_project_menus_header_names_the_host_first() {
        assert_eq!(
            host_header("mini3", Some("Session 0.0.21 · 0.0.22 available")),
            "mini3 · Session 0.0.21 · 0.0.22 available"
        );
        assert_eq!(host_header("mini3", None), "mini3");
    }

    #[test]
    fn only_the_two_must_act_states_draw_a_band_pill() {
        let pill = |state| band_pill(Some(&facts(state, "0.0.21", "0.0.22")));
        assert_eq!(pill(SessionUpdate::Required), Some("needs update"));
        assert_eq!(
            pill(SessionUpdate::SessionNewer { blocked: true }),
            Some("update Roost")
        );
        for state in [
            SessionUpdate::UpToDate,
            SessionUpdate::Available,
            SessionUpdate::Staged,
            SessionUpdate::SessionNewer { blocked: false },
            SessionUpdate::Unordered,
            SessionUpdate::Unknown,
        ] {
            assert_eq!(pill(state), None, "{state:?}");
        }
        assert_eq!(band_pill(None), None);
    }

    #[test]
    fn a_restart_is_verified_by_session_and_by_build() {
        let target = release("0.0.22");
        assert_eq!(verify_restart("a", &target, "b", &target), Ok(()));
        assert_eq!(
            verify_restart("a", &target, "a", &target),
            Err("0.0.22 (the same session)".into())
        );
        assert_eq!(
            verify_restart("a", &target, "b", &release("0.0.21")),
            Err("0.0.21".into())
        );
        // Dev facts are part of the build a restart was aimed at.
        assert_eq!(
            verify_restart("a", &dev("0.0.22", "abc"), "b", &dev("0.0.22", "def")),
            Err("0.0.22 dev def".into())
        );
        assert_eq!(
            describe(&BuildId {
                dev: true,
                ..release("0.0.22")
            }),
            "0.0.22 dev"
        );
    }
}
