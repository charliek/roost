//! The per-host connection state machine, and the compatibility gate
//! that feeds it.
//!
//! Pure: no sockets, no clock, no randomness. The retry delay takes its
//! jitter as an argument and the transitions are ordinary method calls,
//! so every rule below — a build skew a session cannot serve `vt` for
//! is terminal, only localhost auto-retries, the backoff caps — is a
//! unit test rather than a timing experiment.

use std::time::Duration;

use roost_ipc::messages::{AttachPayloadKind, SessionIdentify, SESSION_PROTOCOL_VERSION};
use roost_ui_model::keys::HostId;

/// The payload kinds this client can decode, in the order it offers
/// them to `tab.attach`. A session that advertises none of them has
/// nothing to hand us, whatever else it supports.
///
/// `ghostty-snapshot` leads because it carries what `vt` cannot (the
/// inactive screen, soft-wrap flags, per-cell hyperlinks); `vt` is the
/// build-independent fallback. Which one an attach lands on is the
/// server's to negotiate — this list is only what may be asked for.
pub(crate) const CLIENT_PAYLOAD_KINDS: [&str; 2] =
    [AttachPayloadKind::GHOSTTY_SNAPSHOT, AttachPayloadKind::VT];

/// First retry delay after a mid-session drop.
const BACKOFF_BASE: Duration = Duration::from_millis(250);

/// Ceiling on the retry delay. A session that has been gone for half a
/// minute is not coming back on its own, and a client that keeps dialing
/// every 30 s costs nothing while still noticing when it does.
const BACKOFF_CAP: Duration = Duration::from_secs(30);

/// Which half of the compatibility gate a session failed.
///
/// Three questions, asked in the order that makes the answer useful: a
/// protocol the client cannot speak means nothing else can be trusted,
/// a payload kind it cannot decode means the attach can never work, and
/// only then does the build string matter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MismatchKind {
    /// `session_protocol` is not [`SESSION_PROTOCOL_VERSION`].
    Protocol,
    /// `payload_kinds` contains none of [`CLIENT_PAYLOAD_KINDS`].
    PayloadKind,
    /// `libghostty_build` differs **and** the session cannot serve `vt`
    /// — a session older than the fallback, whose only payload is a
    /// snapshot the two builds cannot exchange.
    Build,
}

/// What the gate found when it did not refuse.
///
/// It reports the *fact* it established, not a decision: which kind an
/// attach ends up on is `tab.attach`'s to negotiate, per attach, and
/// nothing here predicts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Compatibility {
    /// Protocol and libghostty build both match.
    Exact,
    /// The two libghostty builds disagree, and the session serves the
    /// build-independent `vt` payload. Connecting is fine; what a `vt`
    /// payload carries is a documented subset of a terminal, so the
    /// caller says so once, out loud.
    BuildSkew,
}

/// How a saved host is reached — the one structural fact both the
/// reconnect policy and the mismatch dialog's offer are derived from.
///
/// It mirrors [`roost_ipc::ssh::ResolvedTransport`]'s three variants
/// without their payloads, so the two questions that used to be asked
/// as one `localhost: bool` ("is this our own session to spawn and
/// retry?" and "what can we offer when the builds disagree?") cannot
/// drift apart: there is one value, and each is a function of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostTransport {
    /// The `"localhost"` sentinel: this machine's own session.
    LocalSession,
    /// A Unix socket path — somebody else's process, reached directly.
    UnixSocket,
    /// Reached over `ssh`.
    Ssh,
}

impl HostTransport {
    /// Whether this is this machine's own session. Gates the spawn
    /// ladder and the auto-retry policy.
    pub(crate) fn is_localhost(self) -> bool {
        matches!(self, Self::LocalSession)
    }

    /// What this client can offer when the build gate refuses — decided
    /// **structurally**, never probed (plan 039 §3.5).
    pub(crate) fn restart_action(self) -> RestartAction {
        match self {
            Self::LocalSession => RestartAction::RestartLocal,
            Self::Ssh => RestartAction::OfferRemoteUpdate,
            Self::UnixSocket => RestartAction::None,
        }
    }
}

/// What this client can do about a session it cannot talk to.
///
/// Three answers, one per transport, and all three are decided from how
/// the host is reached rather than from anything on the far side — an
/// actual install source is resolved later, at confirm time (plan 039
/// §3.5), so nothing here costs a round trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestartAction {
    /// This machine's own session: stop it and start it again from here.
    RestartLocal,
    /// Reached over `ssh`, so an update *can* be offered — whether a
    /// matching build actually exists to install is resolved when the
    /// user confirms, not now.
    ///
    /// **What a build skew no longer reaches.** This offer is raised
    /// only from `NeedsRestart`, and a session that serves `vt` never
    /// gets there: the host connects on the fallback instead. So a
    /// remote ssh session on an older libghostty has no in-app path to
    /// be updated any more — `roostctl session stop` over ssh and a
    /// fresh Connect is the manual one — until an on-demand
    /// restart/update action exists that does not need a terminal state
    /// to hang off. Accepted deliberately: connecting beats refusing.
    OfferRemoteUpdate,
    /// A remote Unix-socket target: somebody else's process, with no
    /// transport this client could reach the binary over.
    None,
}

/// Everything the upgrade dialog (C8) needs to say what is wrong and
/// what restarting would fix.
///
/// The strings are kept verbatim rather than reduced to a verdict: a
/// user staring at "this session was started by an older Roost" wants
/// to see which two builds disagreed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BuildMismatch {
    pub(crate) kind: MismatchKind,
    pub(crate) session_protocol: u32,
    pub(crate) client_protocol: u32,
    pub(crate) session_build: String,
    pub(crate) client_build: String,
    pub(crate) session_payload_kinds: Vec<String>,
    /// What this client can offer about it — a local restart, a remote
    /// update, or nothing but a pointer at the docs.
    pub(crate) restart: RestartAction,
}

/// Run the compatibility gate against a `session.identify` reply.
///
/// `Ok` means every negotiation this client depends on holds; the error
/// is what the `NeedsRestart` state carries.
pub(crate) fn check_compatibility(
    identity: &SessionIdentify,
    client_build: &str,
    restart: RestartAction,
) -> Result<Compatibility, BuildMismatch> {
    let mismatch = |kind| BuildMismatch {
        kind,
        session_protocol: identity.session_protocol,
        client_protocol: SESSION_PROTOCOL_VERSION,
        session_build: identity.libghostty_build.clone(),
        client_build: client_build.to_string(),
        session_payload_kinds: identity
            .payload_kinds
            .iter()
            .map(|kind| kind.0.clone())
            .collect(),
        restart,
    };

    let serves = |wanted: &str| identity.payload_kinds.iter().any(|kind| kind.0 == wanted);

    if identity.session_protocol != SESSION_PROTOCOL_VERSION {
        return Err(mismatch(MismatchKind::Protocol));
    }
    if !CLIENT_PAYLOAD_KINDS.iter().any(|kind| serves(kind)) {
        return Err(mismatch(MismatchKind::PayloadKind));
    }
    // Exact string match, per `ipc.md` #sessionidentify — a prefix or a
    // "close enough" comparison is how a corrupt screen ships.
    if identity.libghostty_build != client_build {
        // `vt` is a byte stream any VT parser replays, so a session that
        // serves it can be attached to across a build skew — the gate
        // refuses only a session too old to have it, where a snapshot
        // the two builds cannot exchange is all there ever was.
        if !serves(AttachPayloadKind::VT) {
            return Err(mismatch(MismatchKind::Build));
        }
        return Ok(Compatibility::BuildSkew);
    }
    Ok(Compatibility::Exact)
}

/// What one connect attempt learned about the session it reached.
///
/// Facts, not state: they are established during the prologue, published
/// once per incarnation, and read by surfaces that render or report
/// rather than by anything that transitions. They ride beside
/// [`HostConnState`] instead of on it precisely because a state carries
/// what the machine acts on — putting a payload on `Connected` would
/// ripple through every `match` and every exhaustive table for a value
/// no transition depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConnectFacts {
    /// The session's own id, from `session.identify`. It is what binds a
    /// later decision — a resume, a consent card — to the session the
    /// facts describe rather than to whatever is behind the host now.
    pub(crate) session_id: String,
    /// The two libghostty builds. Filled on every connection, skewed or
    /// not, and only *read* when [`Self::reduced_fidelity`] is set: the
    /// reason lines print the pair, and a pair assembled later would
    /// have to re-identify to get it.
    pub(crate) skew: Skew,
    /// The connection is on the build-independent `vt` fallback: links,
    /// the inactive screen and soft-wrap flags are gone until the
    /// session runs a matching build.
    ///
    /// Known at the identify gate, which is why every state-like
    /// surface keys on it rather than on the negotiated payload kind —
    /// that one is `None` until a tab attaches, so a skewed host nobody
    /// has clicked into would show nothing.
    pub(crate) reduced_fidelity: bool,
    /// How this attempt's prologue actually subscribed: `Some` when it
    /// replayed from a carried fence, `None` when it took a fresh
    /// snapshot.
    pub(crate) resumed: Option<ResumeFacts>,
}

/// The two libghostty builds a reduced-fidelity connection sits between.
///
/// Kept verbatim rather than reduced to a verdict for the same reason
/// [`BuildMismatch`] keeps them: a person reading "this session is at
/// reduced fidelity" wants to see which two builds disagreed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Skew {
    pub(crate) session_build: String,
    pub(crate) client_build: String,
}

/// What a resumed prologue replayed from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResumeFacts {
    /// The subscribe **ack's** revision — server-attested, not the fence
    /// the client asked from.
    pub(crate) from_revision: u64,
}

impl ConnectFacts {
    /// Everything the identify gate already established, kept.
    ///
    /// `resumed` is the prologue's to fill once it knows how it
    /// subscribed.
    pub(crate) fn new(
        identity: &SessionIdentify,
        client_build: &str,
        compatibility: Compatibility,
    ) -> Self {
        Self {
            session_id: identity.session_id.clone(),
            skew: Skew {
                session_build: identity.libghostty_build.clone(),
                client_build: client_build.to_string(),
            },
            reduced_fidelity: compatibility == Compatibility::BuildSkew,
            resumed: None,
        }
    }
}

/// Why a host is showing as disconnected, and whether anything is
/// scheduled.
///
/// `retry_in` is the whole auto-reconnect policy on the wire to the UI:
/// `Some` is "we will try again in this long", `None` is "nothing will
/// happen until you ask". A section renders the difference; nothing
/// infers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Disconnected {
    /// One line, shown as-is. "session ended" is an honest reading of a
    /// clean EOF from a localhost session, and the plan requires it be
    /// said rather than dressed up as a transient blip.
    pub(crate) reason: String,
    /// The untruncated text behind `reason`, when `reason` is a band
    /// line too short to carry it — the launch ladder's three rungs, an
    /// exec error, the daemon's own verdict. Only [`HostStateMachine::settled`]
    /// writes it; the band renders `reason` alone and `host.status`
    /// carries this beside it.
    pub(crate) detail: Option<String>,
    pub(crate) retry_in: Option<Duration>,
}

/// Per-host connection lifecycle: `Disconnected → Connecting →
/// Connected → {Stopped, NeedsRestart, Disconnected}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostConnState {
    Disconnected(Disconnected),
    /// An attempt is in flight. `previous` names the incarnation whose
    /// UI state must be purged — the feed tags this event with the *new*
    /// [`HostId`], so a consumer has both halves in one message and can
    /// purge-then-rebuild without keeping a side table.
    Connecting {
        previous: Option<HostId>,
    },
    Connected,
    /// The session said it is shutting down. Terminal for the same
    /// reason — an explicit Connect starts a fresh one.
    Stopped,
    /// The compatibility gate failed. C8 turns this into the upgrade
    /// dialog; C4 only has to make the details available.
    NeedsRestart(BuildMismatch),
}

impl HostConnState {
    /// Whether this connection reached a session and is still on it:
    /// everything a connected host can do is open.
    pub(crate) fn is_connected(&self) -> bool {
        matches!(self, HostConnState::Connected)
    }

    /// Whether this state says the *session* is gone or cannot be talked
    /// to — as opposed to the wire to it, which is what an ordinary
    /// `Disconnected` and every `Connecting` describe.
    ///
    /// It stopped, it needs a restart before this client can speak to
    /// it, or no retry will ever produce one ([`HostStateMachine::settled`], the
    /// only writer of a `Disconnected`'s `detail`). What it gates is
    /// anything held *about the session across connections*: a resume
    /// point offered to a session that restarted names a history that
    /// no longer exists, and revisions restart at zero in every process.
    pub(crate) fn session_is_gone(&self) -> bool {
        match self {
            HostConnState::Stopped | HostConnState::NeedsRestart(_) => true,
            HostConnState::Disconnected(disconnected) => disconnected.detail.is_some(),
            HostConnState::Connecting { .. } | HostConnState::Connected => false,
        }
    }

    /// How this state reads in the sidebar's host band (plan 037 §3.1) —
    /// which dot it paints, whether its rows respond, and what its
    /// rollup says. The mapping lives here so the section model in
    /// `roost-ui-model` stays free of the connection machinery.
    pub(crate) fn section_state(&self) -> roost_ui_model::host_sidebar::SectionState {
        use roost_ui_model::host_sidebar::SectionState;
        match self {
            Self::Disconnected(_) => SectionState::Disconnected,
            Self::Connecting { .. } => SectionState::Connecting,
            Self::Connected => SectionState::Connected,
            Self::Stopped => SectionState::Stopped,
            Self::NeedsRestart(_) => SectionState::NeedsRestart,
        }
    }

    /// Whether an auto-retry is pending, and how long away.
    pub(crate) fn retry_in(&self) -> Option<Duration> {
        match self {
            HostConnState::Disconnected(d) => d.retry_in,
            _ => None,
        }
    }

    /// The long form behind the band line, when this state carries one.
    pub(crate) fn detail(&self) -> Option<&str> {
        match self {
            HostConnState::Disconnected(d) => d.detail.as_deref(),
            _ => None,
        }
    }
}

/// Jittered, capped exponential backoff.
///
/// The jitter is supplied rather than drawn so the cap and the growth
/// are testable; the connection task passes a real random each time.
///
/// The base and the ceiling are per-ladder rather than module constants
/// because two ladders want different first delays and the same growth:
/// [`BACKOFF_BASE`] is right for probing a unix socket on this machine
/// and wrong for a TCP + auth handshake (the SSH reconnect ladder builds
/// its own). [`Default`] is the localhost pair.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Backoff {
    attempt: u32,
    base: Duration,
    cap: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new(BACKOFF_BASE, BACKOFF_CAP)
    }
}

impl Backoff {
    pub(crate) fn new(base: Duration, cap: Duration) -> Self {
        Self {
            attempt: 0,
            base,
            cap,
        }
    }

    /// The delay for the next attempt, and the counter advances.
    ///
    /// `jitter` is clamped to `0.0..=1.0` and spreads the delay over
    /// `[0.5, 1.0] * base * 2^attempt`, capped at this ladder's `cap`.
    /// Full-jitter-down-to-half rather than full-jitter-to-zero: a
    /// storm of clients must spread out, but a single client must not
    /// spin on a socket that is not there.
    pub(crate) fn next_delay(&mut self, jitter: f64) -> Duration {
        let jitter = if jitter.is_finite() {
            jitter.clamp(0.0, 1.0)
        } else {
            0.5
        };
        // Saturating: `2^attempt` overflows long before the cap matters,
        // and a client that has been retrying for hours must still get
        // the ceiling rather than a panic.
        let scale = 1u32.checked_shl(self.attempt.min(31)).unwrap_or(u32::MAX);
        let raw = self
            .base
            .checked_mul(scale)
            .unwrap_or(self.cap)
            .min(self.cap);
        self.attempt = self.attempt.saturating_add(1);
        raw.mul_f64(0.5 + 0.5 * jitter)
    }

    /// A successful connect clears the ladder: the next drop starts over
    /// at the base delay, not wherever the last outage left off.
    pub(crate) fn reset(&mut self) {
        self.attempt = 0;
    }

    #[cfg(test)]
    pub(crate) fn attempt_number(&self) -> u32 {
        self.attempt
    }
}

/// The state machine itself. One per saved host.
///
/// `localhost` is the whole reconnect policy: a localhost session is
/// this machine's own process, so a drop is worth retrying on a timer;
/// any other host is reachable only through something the user set up
/// (an `ssh -L` forward today) and is manual-reconnect only per D8.
#[derive(Debug)]
pub(crate) struct HostStateMachine {
    localhost: bool,
    state: HostConnState,
    backoff: Backoff,
}

impl HostStateMachine {
    pub(crate) fn new(localhost: bool) -> Self {
        Self {
            localhost,
            state: HostConnState::Disconnected(Disconnected {
                reason: "not connected".into(),
                detail: None,
                retry_in: None,
            }),
            backoff: Backoff::default(),
        }
    }

    pub(crate) fn state(&self) -> &HostConnState {
        &self.state
    }

    /// An attempt begins. `previous` is the incarnation being replaced —
    /// `None` on the very first attempt, `Some` on every reconnect.
    pub(crate) fn begin_attempt(&mut self, previous: Option<HostId>) -> HostConnState {
        self.transition(HostConnState::Connecting { previous })
    }

    /// The theme is seeded, the stream is subscribed, the mirror is built.
    pub(crate) fn connected(&mut self) -> HostConnState {
        self.backoff.reset();
        self.transition(HostConnState::Connected)
    }

    /// The compatibility gate refused. Terminal until the user acts —
    /// retrying a build mismatch just reproduces it.
    pub(crate) fn needs_restart(&mut self, mismatch: BuildMismatch) -> HostConnState {
        self.transition(HostConnState::NeedsRestart(mismatch))
    }

    /// A `session.stopping` envelope, or the equivalent op refusal.
    /// Whatever reason the session gave, the answer is the same: stop
    /// driving it rather than retry into something that said goodbye.
    pub(crate) fn stopping(&mut self) -> HostConnState {
        self.transition(HostConnState::Stopped)
    }

    /// The connection dropped for a transport reason (EOF, refused, an
    /// io error). Localhost schedules a retry; anything else waits for
    /// the user.
    pub(crate) fn dropped(&mut self, reason: impl Into<String>, jitter: f64) -> HostConnState {
        let retry_in = self.localhost.then(|| self.backoff.next_delay(jitter));
        self.transition(HostConnState::Disconnected(Disconnected {
            reason: reason.into(),
            detail: None,
            retry_in,
        }))
    }

    /// The connection cannot be established and no retry could change
    /// that — the localhost launch ladder could not produce a daemon
    /// (`task::spawn_failure`). Never schedules a retry **whatever the
    /// transport**: the localhost bool is deliberately not consulted,
    /// because a retry here would dial a socket nothing is going to
    /// create and overwrite this reason with a generic io error every
    /// 250ms. ↻ Reconnect is the recovery.
    pub(crate) fn settled(
        &mut self,
        reason: impl Into<String>,
        detail: impl Into<String>,
    ) -> HostConnState {
        self.transition(HostConnState::Disconnected(Disconnected {
            reason: reason.into(),
            detail: Some(detail.into()),
            retry_in: None,
        }))
    }

    /// The user asked to disconnect. Never schedules a retry, whatever
    /// the host is — asking to disconnect and being reconnected two
    /// seconds later is the one outcome nobody wants.
    pub(crate) fn disconnect_requested(&mut self) -> HostConnState {
        self.backoff.reset();
        self.transition(HostConnState::Disconnected(Disconnected {
            reason: "disconnected".into(),
            detail: None,
            retry_in: None,
        }))
    }

    fn transition(&mut self, next: HostConnState) -> HostConnState {
        self.state = next;
        self.state.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(protocol: u32, kinds: &[&str], build: &str) -> SessionIdentify {
        SessionIdentify {
            app_version: "0.0.18".into(),
            session_protocol: protocol,
            payload_kinds: kinds
                .iter()
                .map(|k| AttachPayloadKind((*k).to_string()))
                .collect(),
            libghostty_build: build.into(),
            session_id: "sess-1".into(),
            started_at: "2026-08-29T00:00:00Z".into(),
        }
    }

    /// The sidebar's three-dot vocabulary, pinned against every
    /// connection state: green while the session is reached, amber while
    /// something is in flight or waiting on the user, grey once the
    /// connection is gone. `is_connected` is the predicate the section
    /// reads, and it is what makes a dimmed section's rows unclickable
    /// everywhere at once (plan 037 §3.1).
    #[test]
    fn every_connection_state_maps_to_a_section_state() {
        use roost_ui_model::host_sidebar::{HostDot, SectionState};

        let dropped = HostConnState::Disconnected(Disconnected {
            reason: "session ended".into(),
            detail: None,
            retry_in: None,
        });
        let mismatch = HostConnState::NeedsRestart(BuildMismatch {
            kind: MismatchKind::Build,
            session_protocol: SESSION_PROTOCOL_VERSION,
            client_protocol: SESSION_PROTOCOL_VERSION,
            session_build: "gb-old".into(),
            client_build: "gb-1".into(),
            session_payload_kinds: vec![AttachPayloadKind::GHOSTTY_SNAPSHOT.to_string()],
            restart: RestartAction::RestartLocal,
        });
        let cases = [
            (HostConnState::Connected, SectionState::Connected),
            (
                HostConnState::Connecting { previous: None },
                SectionState::Connecting,
            ),
            (dropped, SectionState::Disconnected),
            (HostConnState::Stopped, SectionState::Stopped),
            (mismatch, SectionState::NeedsRestart),
        ];
        for (state, expected) in cases {
            assert_eq!(state.section_state(), expected, "{state:?}");
            assert_eq!(
                state.section_state().interactive(),
                state.is_connected(),
                "a host whose session is reached has responsive rows ({state:?})"
            );
        }
        assert_eq!(
            HostConnState::Connected.section_state().dot(),
            HostDot::Connected
        );
        assert_eq!(
            HostConnState::Connecting { previous: None }
                .section_state()
                .dot(),
            HostDot::Pending
        );
        assert_eq!(
            HostConnState::Stopped.section_state().dot(),
            HostDot::Offline
        );
    }

    /// One question: a connection either reached the session and is on
    /// it, or it is not.
    #[test]
    fn only_a_connected_state_is_connected() {
        assert!(HostConnState::Connected.is_connected());
        assert!(!HostConnState::Connected.session_is_gone());

        assert!(!HostConnState::Stopped.is_connected());
        assert!(!HostConnState::Connecting { previous: None }.is_connected());
    }

    #[test]
    fn a_matching_session_passes_every_half_of_the_gate() {
        let ok = identity(SESSION_PROTOCOL_VERSION, &["ghostty-snapshot"], "gb-1");
        assert_eq!(
            check_compatibility(&ok, "gb-1", RestartAction::RestartLocal),
            Ok(Compatibility::Exact)
        );
        // An extra kind the client does not know is not a refusal — the
        // list is open by contract.
        let extra = identity(
            SESSION_PROTOCOL_VERSION,
            &["vt", "ghostty-snapshot", "future"],
            "gb-1",
        );
        assert_eq!(
            check_compatibility(&extra, "gb-1", RestartAction::RestartLocal),
            Ok(Compatibility::Exact)
        );
        // And neither is a session that serves only `vt` — one kind this
        // client can decode is the whole requirement. The gate does not
        // predict which one `tab.attach` will land on.
        let vt_only = identity(SESSION_PROTOCOL_VERSION, &["vt"], "gb-1");
        assert_eq!(
            check_compatibility(&vt_only, "gb-1", RestartAction::RestartLocal),
            Ok(Compatibility::Exact)
        );
    }

    /// The upgrade trap, answered: two libghostty builds that disagree
    /// still connect, because `vt` is not a build-coupled payload. The
    /// gate reports the skew so the connection can say so; it stays a
    /// connection.
    #[test]
    fn a_build_skew_a_session_can_serve_vt_for_connects() {
        let skewed = identity(
            SESSION_PROTOCOL_VERSION,
            &["ghostty-snapshot", "vt"],
            "gb-old",
        );
        assert_eq!(
            check_compatibility(&skewed, "gb-new", RestartAction::RestartLocal),
            Ok(Compatibility::BuildSkew)
        );
    }

    /// What a connection is judged on later: whether it is at reduced
    /// fidelity, and which session it reached. Both are read off the
    /// identify reply the gate already has, and the build pair is kept
    /// whether or not the builds disagree — the reason lines that print
    /// it cannot go back and ask.
    #[test]
    fn the_prologues_facts_come_off_the_identify_reply() {
        let matched = identity(SESSION_PROTOCOL_VERSION, &["ghostty-snapshot"], "gb-1");
        let exact = ConnectFacts::new(&matched, "gb-1", Compatibility::Exact);
        assert!(!exact.reduced_fidelity);
        assert_eq!(exact.session_id, "sess-1");
        assert_eq!(
            exact.skew,
            Skew {
                session_build: "gb-1".into(),
                client_build: "gb-1".into(),
            },
            "the pair is filled on every connection, not only a skewed one"
        );
        assert_eq!(exact.resumed, None, "nothing has subscribed yet");

        let skewed = identity(SESSION_PROTOCOL_VERSION, &["vt"], "gb-old");
        let reduced = ConnectFacts::new(&skewed, "gb-new", Compatibility::BuildSkew);
        assert!(reduced.reduced_fidelity);
        assert_eq!(reduced.skew.session_build, "gb-old");
        assert_eq!(reduced.skew.client_build, "gb-new");
    }

    /// A session from before `vt` existed has only the build-coupled
    /// payload, so the same skew is terminal there — the pre-R3 daemon
    /// the restart flow is still for.
    #[test]
    fn a_build_skew_without_vt_is_still_terminal() {
        let pre_vt = identity(SESSION_PROTOCOL_VERSION, &["ghostty-snapshot"], "gb-old");
        assert_eq!(
            check_compatibility(&pre_vt, "gb-new", RestartAction::RestartLocal)
                .unwrap_err()
                .kind,
            MismatchKind::Build
        );
    }

    #[test]
    fn each_half_of_the_gate_names_itself() {
        let wrong_protocol = identity(1, &["ghostty-snapshot"], "gb-1");
        assert_eq!(
            check_compatibility(&wrong_protocol, "gb-1", RestartAction::RestartLocal)
                .unwrap_err()
                .kind,
            MismatchKind::Protocol
        );

        let no_kind = identity(SESSION_PROTOCOL_VERSION, &["sixel-mosaic"], "gb-1");
        assert_eq!(
            check_compatibility(&no_kind, "gb-1", RestartAction::RestartLocal)
                .unwrap_err()
                .kind,
            MismatchKind::PayloadKind
        );

        let wrong_build = identity(SESSION_PROTOCOL_VERSION, &["ghostty-snapshot"], "gb-2");
        let mismatch = check_compatibility(&wrong_build, "gb-1", RestartAction::None).unwrap_err();
        assert_eq!(mismatch.kind, MismatchKind::Build);
        assert_eq!(mismatch.session_build, "gb-2");
        assert_eq!(mismatch.client_build, "gb-1");
        assert_eq!(
            mismatch.restart,
            RestartAction::None,
            "a remote socket session is not ours to restart, and there is no \
             transport to update it over either"
        );
    }

    /// The protocol check comes first: a session speaking a version we
    /// do not know may not even mean the same thing by the other fields.
    #[test]
    fn the_protocol_check_wins_over_the_build_check() {
        let both_wrong = identity(99, &["vt"], "gb-2");
        assert_eq!(
            check_compatibility(&both_wrong, "gb-1", RestartAction::RestartLocal)
                .unwrap_err()
                .kind,
            MismatchKind::Protocol
        );
    }

    #[test]
    fn the_happy_path_walks_disconnected_connecting_connected() {
        let mut machine = HostStateMachine::new(true);
        assert!(matches!(machine.state(), HostConnState::Disconnected(_)));

        assert_eq!(
            machine.begin_attempt(None),
            HostConnState::Connecting { previous: None }
        );
        assert_eq!(machine.connected(), HostConnState::Connected);
        assert!(machine.state().is_connected());
    }

    /// The reconnect contract: the transition into `Connecting` is what
    /// carries the dead incarnation, so a consumer purges and re-derives
    /// off one message.
    #[test]
    fn a_reconnect_names_the_incarnation_it_replaces() {
        let mut machine = HostStateMachine::new(true);
        machine.begin_attempt(None);
        machine.connected();
        machine.dropped("eof", 0.5);

        let previous = HostId::new(3);
        assert_eq!(
            machine.begin_attempt(Some(previous)),
            HostConnState::Connecting {
                previous: Some(previous)
            }
        );
    }

    /// The fixture is a session from before `vt` — the one build skew
    /// that still reaches this state at all.
    #[test]
    fn a_build_mismatch_is_terminal_and_carries_its_details() {
        let mut machine = HostStateMachine::new(true);
        machine.begin_attempt(None);
        let mismatch = check_compatibility(
            &identity(SESSION_PROTOCOL_VERSION, &["ghostty-snapshot"], "gb-old"),
            "gb-new",
            RestartAction::RestartLocal,
        )
        .unwrap_err();

        let state = machine.needs_restart(mismatch.clone());
        assert_eq!(state, HostConnState::NeedsRestart(mismatch));
        assert!(state.retry_in().is_none(), "a mismatch never auto-retries");
    }

    /// A client that keeps driving a session that said goodbye is the
    /// failure this transition exists to prevent, whatever the session
    /// gave as its reason.
    #[test]
    fn a_stopping_session_stops_this_connection() {
        let mut machine = HostStateMachine::new(true);
        machine.begin_attempt(None);
        machine.connected();
        assert_eq!(machine.stopping(), HostConnState::Stopped);
    }

    #[test]
    fn only_localhost_schedules_its_own_retry() {
        let mut local = HostStateMachine::new(true);
        local.begin_attempt(None);
        local.connected();
        assert!(local.dropped("session ended", 1.0).retry_in().is_some());

        let mut remote = HostStateMachine::new(false);
        remote.begin_attempt(None);
        remote.connected();
        let state = remote.dropped("connection reset", 1.0);
        assert!(
            state.retry_in().is_none(),
            "a non-localhost host is manual-reconnect only"
        );
    }

    /// The one transition that ignores the localhost bool. A launch
    /// failure on the *most* retryable transport there is still settles,
    /// because the retry would dial a socket nothing is left to create —
    /// so this is asserted on a localhost machine whose backoff has
    /// already advanced, which is the shape a real settle arrives in.
    #[test]
    fn a_settled_localhost_never_schedules_a_retry() {
        let mut machine = HostStateMachine::new(true);
        machine.begin_attempt(None);
        assert!(
            machine.dropped("session ended", 1.0).retry_in().is_some(),
            "the fixture only means something if this transport does retry"
        );

        let state = machine.settled("cannot find roost-session", "the three rungs, verbatim");
        assert_eq!(
            state,
            HostConnState::Disconnected(Disconnected {
                reason: "cannot find roost-session".into(),
                detail: Some("the three rungs, verbatim".into()),
                retry_in: None,
            })
        );
    }

    #[test]
    fn an_explicit_disconnect_never_schedules_a_retry() {
        let mut machine = HostStateMachine::new(true);
        machine.begin_attempt(None);
        machine.connected();
        assert!(machine.disconnect_requested().retry_in().is_none());
    }

    #[test]
    fn the_backoff_grows_then_caps() {
        let mut backoff = Backoff::default();
        // Maximum jitter isolates the growth from the spread.
        let delays: Vec<Duration> = (0..12).map(|_| backoff.next_delay(1.0)).collect();
        assert_eq!(delays[0], BACKOFF_BASE);
        assert_eq!(delays[1], BACKOFF_BASE * 2);
        for pair in delays.windows(2) {
            assert!(pair[1] >= pair[0], "{pair:?} went backwards");
        }
        assert_eq!(*delays.last().unwrap(), BACKOFF_CAP);
        for delay in &delays {
            assert!(*delay <= BACKOFF_CAP, "{delay:?} exceeded the cap");
        }
    }

    /// Hours of retrying must reach the ceiling, not overflow into one.
    #[test]
    fn a_very_long_outage_stays_at_the_ceiling() {
        let mut backoff = Backoff::default();
        for _ in 0..10_000 {
            let delay = backoff.next_delay(1.0);
            assert!(delay <= BACKOFF_CAP, "{delay:?}");
        }
        assert_eq!(backoff.next_delay(1.0), BACKOFF_CAP);
    }

    #[test]
    fn jitter_spreads_within_half_the_delay_and_never_reaches_zero() {
        for jitter in [0.0, 0.25, 0.5, 1.0, f64::NAN, -3.0, 7.0] {
            let mut backoff = Backoff::default();
            let delay = backoff.next_delay(jitter);
            assert!(
                delay >= BACKOFF_BASE / 2 && delay <= BACKOFF_BASE,
                "{jitter} produced {delay:?}"
            );
        }
    }

    /// Parameterizing the base and the cap must not move localhost's
    /// ladder: the default pair is still 250ms doubling to 30s, spelled
    /// as literals so a stray edit to either constant is caught here
    /// rather than in a reconnect that suddenly takes a minute.
    #[test]
    fn the_default_ladder_is_still_localhosts_250ms_to_30s() {
        let mut backoff = Backoff::default();
        assert_eq!(backoff.next_delay(1.0), Duration::from_millis(250));
        assert_eq!(backoff.next_delay(1.0), Duration::from_millis(500));
        for _ in 0..20 {
            backoff.next_delay(1.0);
        }
        assert_eq!(backoff.next_delay(1.0), Duration::from_secs(30));
    }

    /// A ladder built with its own pair grows off that base and settles
    /// on that ceiling — the seam the SSH reconnect policy needs.
    #[test]
    fn a_constructed_ladder_uses_the_base_and_cap_it_was_given() {
        let mut backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(4));
        let delays: Vec<Duration> = (0..5).map(|_| backoff.next_delay(1.0)).collect();
        assert_eq!(
            delays,
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(4),
                Duration::from_secs(4),
            ]
        );
    }

    #[test]
    fn a_successful_connect_resets_the_ladder() {
        let mut machine = HostStateMachine::new(true);
        for _ in 0..5 {
            machine.dropped("eof", 1.0);
        }
        assert!(machine.backoff.attempt_number() > 0);
        machine.connected();
        assert_eq!(machine.backoff.attempt_number(), 0);
        assert_eq!(
            machine.dropped("eof", 1.0).retry_in(),
            Some(BACKOFF_BASE),
            "the next outage starts at the base delay again"
        );
    }
}
