//! The per-host connection state machine, and the compatibility gate
//! that feeds it.
//!
//! Pure: no sockets, no clock, no randomness. The retry delay takes its
//! jitter as an argument and the transitions are ordinary method calls,
//! so every rule below — a build mismatch is terminal, a takeover is
//! terminal, only localhost auto-retries, the backoff caps — is a unit
//! test rather than a timing experiment.

use std::time::Duration;

use roost_ipc::messages::{AttachPayloadKind, SessionIdentify, SESSION_PROTOCOL_VERSION};
use roost_ui_model::keys::HostId;

/// The payload kind the client can actually decode. A session that
/// cannot offer it has nothing to hand us, whatever else it supports.
pub(crate) const REQUIRED_PAYLOAD_KIND: &str = AttachPayloadKind::GHOSTTY_SNAPSHOT;

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
    /// `payload_kinds` does not contain [`REQUIRED_PAYLOAD_KIND`].
    PayloadKind,
    /// `libghostty_build` differs. Two libghostty builds that disagree
    /// cannot exchange a snapshot — the upgrade flow's common case.
    Build,
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
/// `Ok(())` means every negotiation this client depends on holds; the
/// error is what the `NeedsRestart` state carries.
pub(crate) fn check_compatibility(
    identity: &SessionIdentify,
    client_build: &str,
    restart: RestartAction,
) -> Result<(), BuildMismatch> {
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

    if identity.session_protocol != SESSION_PROTOCOL_VERSION {
        return Err(mismatch(MismatchKind::Protocol));
    }
    if !identity
        .payload_kinds
        .iter()
        .any(|kind| kind.0 == REQUIRED_PAYLOAD_KIND)
    {
        return Err(mismatch(MismatchKind::PayloadKind));
    }
    // Exact string match, per `ipc.md` #sessionidentify — a prefix or a
    // "close enough" comparison is how a corrupt screen ships.
    if identity.libghostty_build != client_build {
        return Err(mismatch(MismatchKind::Build));
    }
    Ok(())
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
    pub(crate) retry_in: Option<Duration>,
}

/// Per-host connection lifecycle: `Disconnected → Connecting →
/// Connected → {TakenOver, Stopped, NeedsRestart, Disconnected}`.
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
    /// Another client took the lease. Terminal: nothing auto-retries,
    /// because retrying is taking it back and that is a decision.
    TakenOver,
    /// The session said it is shutting down. Terminal for the same
    /// reason — an explicit Connect starts a fresh one.
    Stopped,
    /// The compatibility gate failed. C8 turns this into the upgrade
    /// dialog; C4 only has to make the details available.
    NeedsRestart(BuildMismatch),
}

impl HostConnState {
    pub(crate) fn is_connected(&self) -> bool {
        matches!(self, HostConnState::Connected)
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
            Self::TakenOver => SectionState::TakenOver,
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
}

/// Jittered, capped exponential backoff.
///
/// The jitter is supplied rather than drawn so the cap and the growth
/// are testable; the connection task passes a real random each time.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Backoff {
    attempt: u32,
}

impl Backoff {
    /// The delay for the next attempt, and the counter advances.
    ///
    /// `jitter` is clamped to `0.0..=1.0` and spreads the delay over
    /// `[0.5, 1.0] * base * 2^attempt`, capped at [`BACKOFF_CAP`].
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
        let raw = BACKOFF_BASE
            .checked_mul(scale)
            .unwrap_or(BACKOFF_CAP)
            .min(BACKOFF_CAP);
        self.attempt = self.attempt.saturating_add(1);
        raw.mul_f64(0.5 + 0.5 * jitter)
    }

    /// A successful connect clears the ladder: the next drop starts over
    /// at [`BACKOFF_BASE`], not wherever the last outage left off.
    pub(crate) fn reset(&mut self) {
        self.attempt = 0;
    }

    #[cfg(test)]
    pub(crate) fn attempt(&self) -> u32 {
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

    /// The lease is held, the theme is seeded, the mirror is built.
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
    /// `"taken-over"` and `"stop"` are the two the wire defines; an
    /// unrecognized reason is read as a stop, which is the safe half —
    /// it stops driving rather than silently retrying into a session
    /// that told us it was going away.
    pub(crate) fn stopping(&mut self, reason: &str) -> HostConnState {
        let next = if reason == "taken-over" {
            HostConnState::TakenOver
        } else {
            HostConnState::Stopped
        };
        self.transition(next)
    }

    /// The connection dropped for a transport reason (EOF, refused, an
    /// io error). Localhost schedules a retry; anything else waits for
    /// the user.
    pub(crate) fn dropped(&mut self, reason: impl Into<String>, jitter: f64) -> HostConnState {
        let retry_in = self.localhost.then(|| self.backoff.next_delay(jitter));
        self.transition(HostConnState::Disconnected(Disconnected {
            reason: reason.into(),
            retry_in,
        }))
    }

    /// The user asked to disconnect. Never schedules a retry, whatever
    /// the host is — asking to disconnect and being reconnected two
    /// seconds later is the one outcome nobody wants.
    pub(crate) fn disconnect_requested(&mut self) -> HostConnState {
        self.backoff.reset();
        self.transition(HostConnState::Disconnected(Disconnected {
            reason: "disconnected".into(),
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
    /// connection state: green only while actually connected, amber while
    /// something is in flight or waiting on the user, grey once the
    /// connection is gone. And only a connected section is interactive —
    /// which is what makes a dimmed section's rows unclickable everywhere
    /// at once (plan 037 §3.1).
    #[test]
    fn every_connection_state_maps_to_a_section_state() {
        use roost_ui_model::host_sidebar::{HostDot, SectionState};

        let dropped = HostConnState::Disconnected(Disconnected {
            reason: "session ended".into(),
            retry_in: None,
        });
        let mismatch = HostConnState::NeedsRestart(BuildMismatch {
            kind: MismatchKind::Build,
            session_protocol: SESSION_PROTOCOL_VERSION,
            client_protocol: SESSION_PROTOCOL_VERSION,
            session_build: "gb-old".into(),
            client_build: "gb-1".into(),
            session_payload_kinds: vec![REQUIRED_PAYLOAD_KIND.to_string()],
            restart: RestartAction::RestartLocal,
        });
        let cases = [
            (HostConnState::Connected, SectionState::Connected),
            (
                HostConnState::Connecting { previous: None },
                SectionState::Connecting,
            ),
            (dropped, SectionState::Disconnected),
            (HostConnState::TakenOver, SectionState::TakenOver),
            (HostConnState::Stopped, SectionState::Stopped),
            (mismatch, SectionState::NeedsRestart),
        ];
        for (state, expected) in cases {
            assert_eq!(state.section_state(), expected, "{state:?}");
            assert_eq!(
                state.section_state().interactive(),
                state.is_connected(),
                "only a connected host's rows respond ({state:?})"
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
            HostConnState::TakenOver.section_state().dot(),
            HostDot::Offline
        );
    }

    #[test]
    fn a_matching_session_passes_every_half_of_the_gate() {
        let ok = identity(SESSION_PROTOCOL_VERSION, &["ghostty-snapshot"], "gb-1");
        assert_eq!(
            check_compatibility(&ok, "gb-1", RestartAction::RestartLocal),
            Ok(())
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
            Ok(())
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

        let no_kind = identity(SESSION_PROTOCOL_VERSION, &["vt"], "gb-1");
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

    #[test]
    fn a_taken_over_stop_reason_is_its_own_state() {
        let mut machine = HostStateMachine::new(true);
        machine.begin_attempt(None);
        machine.connected();
        assert_eq!(machine.stopping("taken-over"), HostConnState::TakenOver);

        let mut machine = HostStateMachine::new(true);
        machine.begin_attempt(None);
        machine.connected();
        assert_eq!(machine.stopping("stop"), HostConnState::Stopped);
    }

    /// An unrecognized reason must not be read as a takeover — a client
    /// that keeps driving a session that said goodbye is the failure
    /// this arm exists to prevent.
    #[test]
    fn an_unknown_stop_reason_reads_as_a_stop() {
        let mut machine = HostStateMachine::new(true);
        assert_eq!(machine.stopping("something-new"), HostConnState::Stopped);
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

    #[test]
    fn a_successful_connect_resets_the_ladder() {
        let mut machine = HostStateMachine::new(true);
        for _ in 0..5 {
            machine.dropped("eof", 1.0);
        }
        assert!(machine.backoff.attempt() > 0);
        machine.connected();
        assert_eq!(machine.backoff.attempt(), 0);
        assert_eq!(
            machine.dropped("eof", 1.0).retry_in(),
            Some(BACKOFF_BASE),
            "the next outage starts at the base delay again"
        );
    }
}
