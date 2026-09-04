//! The edges where `App` state meets [`HostConnSet`] — dial, due, land.
//!
//! Every function here is a decision over the narrowest constructible
//! cluster it reads: the [`ExitState`] latch, the connection set, and
//! the in-flight bootstrap registry. `App` keeps the delegation and the
//! window/workspace/task tail that follows the decision — a `set_status`
//! toast, a spawned probe, a focus push — because none of that is what a
//! unit test wants to drive (#383, plan 045 §3.2).
//!
//! What that buys: the three shutdown gates and the dial's probe cancel
//! are each fenced by a test with a negative control, where before they
//! were reachable only through `App::bootstrap` — which measures fonts,
//! builds a runtime, hydrates the workspace and binds a socket.

use super::*;

use crate::host_conn::{
    AttemptCause, ConnectMode, HostConnSet, HostTransport, HostTunnelReady, RequestOrigin,
};
use roost_engine::persistence::HostSnapshot;
use roost_ipc::ssh::ResolvedTransport;

use self::bootstrap::BootstrapsInFlight;

/// An armed auto-reconnect came due — dial, and *as what*?
///
/// The gate is here rather than in [`HostConnSet::reconnect_due`]
/// because `EngineFeed::Quit` does not stop the drain: a due message
/// sitting behind one would re-enter the connect path after the user
/// asked to quit. Refusing before the set is asked also leaves the armed
/// rung where it is — the set's own answer *consumes* it.
pub(super) fn reconnect_due(
    exit: ExitState,
    hosts: &mut HostConnSet,
    saved_id: &str,
    request: u64,
) -> Option<(RequestOrigin, AttemptCause)> {
    if exit != ExitState::Running {
        tracing::debug!(host = %saved_id, "not reconnecting a host during shutdown");
        return None;
    }
    hosts.reconnect_due(saved_id, request)
}

/// What a finished establish did, once the shutdown gate has had its
/// say.
///
/// `Discarded` means **the gate fired and nothing else**. Every response
/// that reaches a running app is `Landed`, including a stale or
/// superseded one that [`HostConnSet::tunnel_ready`] drops internally
/// and answers `None` for: the app still asks after a bootstrap offer on
/// such a response, and folding it into `Discarded` would change that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TunnelLanding {
    Discarded,
    Landed {
        saved_id: String,
        /// The status line this attempt owes the user, if any.
        reason: Option<String>,
    },
}

/// An ssh tunnel finished coming up, or failed to. Dialing is the set's;
/// the toast and the offer are the app's.
///
/// The same guard [`reconnect_due`] carries, for the window it cannot
/// see (plan 040 §3.4): `EngineFeed::Quit` only latches the exit, so an
/// establish that lands behind it in the same drain would otherwise dial
/// a session while the app is shutting down — and `abandon_reconnects`,
/// which runs after the drain, has nothing left to abort by then. The
/// tunnel is still retired properly rather than dropped.
pub(super) fn tunnel_ready(
    exit: ExitState,
    hosts: &mut HostConnSet,
    ready: HostTunnelReady,
) -> TunnelLanding {
    if exit != ExitState::Running {
        tracing::debug!(host = %ready.host, "not connecting a host during shutdown");
        hosts.discard_ready(ready);
        return TunnelLanding::Discarded;
    }
    let saved_id = ready.host.clone();
    let reason = hosts.tunnel_ready(ready);
    TunnelLanding::Landed { saved_id, reason }
}

/// Classify a saved host's target and hand it to the connection set.
/// `mode` answers what to do with a resolved target, and `None` declines
/// the connection — which is how the launch probe skips a remote host
/// without duplicating the classification.
///
/// Three transports, two shapes. A local session and a socket path both
/// resolve to a socket that already exists, so they dial straight away.
/// An ssh target has no socket until a tunnel binds one, so it goes
/// through [`HostConnSet::open_ssh`] and reaches `connect` one feed item
/// later — see [`crate::engine_feed::EngineFeed::HostTunnel`].
///
/// Every dial route ends here, which is why the shutdown gate is here
/// too. `abandon_reconnects` rides the `ExitState` latch and so sweeps
/// exactly once; a bootstrap job whose success arm asks for a reconnect,
/// or an IPC `host.connect` serviced in a later `update`, would
/// otherwise establish a fresh `ControlPersist` master that outlives the
/// app. `App`'s two earlier guards stay where they are — they
/// short-circuit before doing other work.
///
/// `policy` is a parameter rather than a [`host_verbs::VerbPolicy::current`]
/// read: it is state the decision turns on, and state a decision turns on
/// is passed in.
// Eight parameters is one over clippy's bar, and each is a cluster this
// dial genuinely reads — bundling any of them into a struct would only
// move the argument list somewhere less legible.
#[allow(clippy::too_many_arguments)]
pub(super) fn dial_saved_host(
    exit: ExitState,
    hosts: &mut HostConnSet,
    bootstraps: &mut BootstrapsInFlight,
    host: &HostSnapshot,
    origin: RequestOrigin,
    mode: impl FnOnce(bool) -> Option<ConnectMode>,
    cause: AttemptCause,
    policy: host_verbs::VerbPolicy,
) {
    if exit != ExitState::Running {
        tracing::debug!(host = %host.id, "not connecting a host during shutdown");
        return;
    }
    // A new attempt replaces the origin and the failure an in-flight
    // probe's question was built on, so that probe is now asking about
    // something that is no longer happening: user Connect → probe out →
    // an IPC `host.connect` supersedes it → a consent card raised at
    // nobody. Superseding the probe is not enough — nothing would
    // re-arm it.
    cancel_bootstrap_probe(hosts, bootstraps, &host.id);
    let transport = match roost_ipc::ssh::classify(&host.target) {
        Ok(transport) => transport,
        Err(error) => {
            tracing::warn!(host = %host.id, ?error, "cannot resolve a saved host's target");
            return;
        }
    };
    let localhost = transport.is_localhost();
    let Some(mode) = mode(localhost) else {
        return;
    };
    let mode = spawn_gate(mode, policy);
    // The one place the transport becomes the connection set's own
    // vocabulary. Everything downstream that used to ask "is this
    // localhost?" — the spawn ladder, the auto-retry policy, and now
    // what a build mismatch can offer — reads it off this value, so the
    // three answers cannot disagree.
    match transport {
        ResolvedTransport::LocalSession(socket) => hosts.connect(
            &host.id,
            &host.label,
            socket,
            HostTransport::LocalSession,
            mode,
            cause,
        ),
        ResolvedTransport::UnixSocket(socket) => hosts.connect(
            &host.id,
            &host.label,
            socket,
            HostTransport::UnixSocket,
            mode,
            cause,
        ),
        ResolvedTransport::Ssh(target) => {
            hosts.open_ssh(&host.id, &host.label, target, mode, origin, cause)
        }
    }
}

/// Drop an in-flight probe, and the band line it left. Answers whether
/// there was one.
///
/// Called wherever a connect or a disconnect begins: both replace the
/// state the probe's question was asked about, so its answer can only
/// describe something that has already stopped being true.
pub(super) fn cancel_bootstrap_probe(
    hosts: &mut HostConnSet,
    bootstraps: &mut BootstrapsInFlight,
    saved_id: &str,
) -> bool {
    if !bootstraps.cancel_probe(saved_id) {
        return false;
    }
    tracing::debug!(host = %saved_id, "a new attempt superseded a bootstrap probe");
    hosts.set_bootstrap_note(saved_id, None);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use crate::host_conn::fixtures::{
        a_dropped_ssh_host, a_set, abort_establishes, an_unestablished_tunnel, ssh_target,
    };
    use crate::host_conn::state::HostConnState;

    /// A saved host reached over ssh, as `state.json` carries one.
    fn an_ssh_host() -> HostSnapshot {
        HostSnapshot {
            id: "h1".to_string(),
            label: "one".to_string(),
            target: "workbox".to_string(),
            last_connected: None,
        }
    }

    /// One `open_ssh` as the app drives it, with nothing riding on the
    /// origin or the cause.
    fn ask_ssh(set: &mut HostConnSet) {
        set.open_ssh(
            "h1",
            "one",
            ssh_target("workbox"),
            ConnectMode::Dial,
            RequestOrigin::User,
            AttemptCause::Explicit,
        );
    }

    /// F1. A quit latched while a retry timer was armed: the timer still
    /// fires, and its due message still arrives — the drain does not
    /// stop at `EngineFeed::Quit`. Refusing it here is what keeps the
    /// ladder from dialing a host on the way out, and refusing *before*
    /// the set is asked is what leaves the rung armed rather than
    /// spending it on a dial that never happens.
    #[tokio::test]
    async fn a_due_reconnect_is_refused_while_exiting() {
        let (mut set, _feed) = a_set();
        a_dropped_ssh_host(&mut set, "/nonexistent/roost-lifecycle-due.sock");
        abort_establishes(&mut set);
        let request = set.ssh_request("h1");
        assert!(set.outage_armed("h1"), "the fixture leaves a rung armed");

        assert_eq!(
            reconnect_due(ExitState::Requested, &mut set, "h1", request),
            None,
            "a due reconnect behind a quit must not authorize a dial"
        );
        assert!(
            set.has_outage("h1") && set.outage_armed("h1"),
            "and the refusal leaves the armed rung unspent"
        );

        // The positive control, on the same rung: nothing about the
        // ladder's own state was what refused it.
        assert_eq!(
            reconnect_due(ExitState::Running, &mut set, "h1", request),
            Some((RequestOrigin::Ipc, AttemptCause::AutoReconnect)),
            "the same due message dials while the app is running"
        );
    }

    /// F2. The hazard is the `Ok` half, not the failure: an establish
    /// that *came up* behind a quit is one that would dial a session the
    /// app is in the middle of leaving, and open a fresh
    /// `ControlPersist` master that outlives the process. The tunnel is
    /// retired on the engine runtime rather than dropped, which the
    /// scratch directory's removal measures.
    #[tokio::test]
    async fn a_tunnel_landing_behind_a_quit_is_discarded_not_dialed() {
        let (mut set, _feed) = a_set();
        ask_ssh(&mut set);
        // Nothing in this suite may dial a real host, and this case has
        // to let the runtime run: the awaits below would otherwise poll
        // the handshake `open_ssh` spawned.
        abort_establishes(&mut set);
        let request = set.ssh_request("h1");

        let parent = tempfile::Builder::new()
            .prefix("roost-lifecycle-quit")
            .tempdir()
            .expect("a scratch parent");
        let tunnel = an_unestablished_tunnel(parent.path().to_path_buf()).await;
        let scratch = tunnel
            .bridge_socket()
            .parent()
            .expect("a tunnel's socket sits in its scratch directory")
            .to_path_buf();

        let landing = tunnel_ready(
            ExitState::Requested,
            &mut set,
            HostTunnelReady {
                host: "h1".to_string(),
                request,
                result: Ok(tunnel),
            },
        );

        assert_eq!(
            landing,
            TunnelLanding::Discarded,
            "a tunnel that comes up behind a quit is the app's to drop, not to land"
        );
        assert!(!set.has_conn("h1"), "and it must not have dialed a session");
        assert!(set.is_empty(), "the set holds no live connection at all");
        // Retired rather than dropped: the `-O exit` and this removal
        // are the difference between a discard and a leak.
        tokio::time::timeout(Duration::from_secs(5), async {
            while scratch.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the discarded tunnel's scratch directory is removed");

        // The positive control, on a set the quit never reached.
        let (mut running, _running_feed) = a_set();
        ask_ssh(&mut running);
        abort_establishes(&mut running);
        let request = running.ssh_request("h1");
        let parent = tempfile::Builder::new()
            .prefix("roost-lifecycle-running")
            .tempdir()
            .expect("a scratch parent");
        let tunnel = an_unestablished_tunnel(parent.path().to_path_buf()).await;

        let landing = tunnel_ready(
            ExitState::Running,
            &mut running,
            HostTunnelReady {
                host: "h1".to_string(),
                request,
                result: Ok(tunnel),
            },
        );

        assert_eq!(
            landing,
            TunnelLanding::Landed {
                saved_id: "h1".to_string(),
                reason: None,
            },
            "a success says nothing out loud — the state machine speaks for it"
        );
        assert!(
            matches!(running.state("h1"), Some(HostConnState::Connecting { .. })),
            "and the same tunnel dials while the app is running"
        );
        assert!(running.has_conn("h1"));
    }

    /// F3. Every dial route ends at `dial_saved_host`, which is why the
    /// shutdown gate is there: an IPC `host.connect` serviced in a later
    /// `update`, or a bootstrap job's success arm asking for a
    /// reconnect, both arrive after the latch is up.
    #[tokio::test]
    async fn a_dial_is_refused_while_exiting() {
        let (mut set, _feed) = a_set();
        let mut bootstraps = BootstrapsInFlight::default();
        let host = an_ssh_host();

        dial_saved_host(
            ExitState::Requested,
            &mut set,
            &mut bootstraps,
            &host,
            RequestOrigin::User,
            |_| Some(ConnectMode::Dial),
            AttemptCause::Explicit,
            host_verbs::VerbPolicy::current(),
        );

        assert!(
            !set.establishing("h1"),
            "a dial behind a quit must not open an ssh establish"
        );
        assert!(set.is_empty(), "nor a connection");

        // The positive control: the same call under a running app is the
        // dial this refused.
        dial_saved_host(
            ExitState::Running,
            &mut set,
            &mut bootstraps,
            &host,
            RequestOrigin::User,
            |_| Some(ConnectMode::Dial),
            AttemptCause::Explicit,
            host_verbs::VerbPolicy::current(),
        );
        assert!(set.establishing("h1"), "the establish is under way");
        abort_establishes(&mut set);
    }

    /// F4. A probe's question is built on the origin and the failure of
    /// the attempt that raised it, and a new dial replaces both — so the
    /// answer could only describe something that has stopped being true:
    /// user Connect → probe out → an IPC `host.connect` supersedes it →
    /// a consent card raised at nobody.
    ///
    /// The band note is not the observable: `open_ssh` clears that
    /// itself, so it reads right even with the cancel gone. The probe
    /// registry is what only the cancel touches.
    #[tokio::test]
    async fn a_dial_supersedes_an_outstanding_probe() {
        let (mut set, _feed) = a_set();
        let mut bootstraps = BootstrapsInFlight::default();
        bootstraps.begin_probe("h1", 7);
        set.set_bootstrap_note("h1", Some("checking one…".to_string()));

        dial_saved_host(
            ExitState::Running,
            &mut set,
            &mut bootstraps,
            &an_ssh_host(),
            RequestOrigin::Ipc,
            |_| Some(ConnectMode::Dial),
            AttemptCause::Explicit,
            host_verbs::VerbPolicy::current(),
        );

        assert!(
            !bootstraps.probing("h1"),
            "the dial that replaced the probe's question must also drop the probe"
        );
        assert_eq!(
            set.section_reason("h1"),
            None,
            "and the band line the probe left goes with it"
        );
        abort_establishes(&mut set);
    }
}
