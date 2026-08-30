//! The client-side restart composition (plan 037 §3.7).
//!
//! There is no `session.restart` op and there is deliberately not going
//! to be one: restarting is three things the wire already does, in an
//! order only a client can hold. Ask the session to stop; wait until it
//! has really gone (it unlinks its socket in a finalizer that runs
//! *after* the reply, so the reply is not the end); then connect again,
//! which climbs the shared launch ladder and starts one if none is
//! listening. The layout survives because the session hydrates its own
//! `state.json` — every tab reopens as a fresh shell in its directory,
//! which is exactly what the dialog warns about.
//!
//! The first two rungs live in `roost_ipc::session_launch` beside the
//! launch ladder, shared verbatim with `roostctl session stop`. What is
//! here is the *order*, the budgets, and — the part that earns a module
//! — naming the step that failed, because "restart failed" is useless
//! and "could not stop the session" is not.

use std::path::{Path, PathBuf};

use roost_ipc::session_launch;

/// The restarts this client has under way, by saved host id.
///
/// A restart is a ladder of socket work with minute-scale budgets, and
/// the prompt that starts it is still reachable while it runs — the host
/// stays in `NeedsRestart` until the relaunch connects, so a second
/// Connect re-raises the same dialog. Two ladders on one socket race:
/// the second `session.stop` can land on the session the first one's
/// relaunch just spawned, and the two spawns then fight over the bind.
///
/// So the ladder is claimed, not merely started. `HashSet` rather than a
/// flag because two hosts may legitimately restart at once.
#[derive(Debug, Default)]
pub(crate) struct RestartsInFlight(std::collections::HashSet<String>);

impl RestartsInFlight {
    /// Claim the ladder for a host. `false` when one is already running,
    /// which makes the second press a no-op rather than a second
    /// stop+relaunch.
    pub(crate) fn begin(&mut self, host: &str) -> bool {
        self.0.insert(host.to_string())
    }

    /// Release the claim. Called on **every** outcome — a ladder that
    /// failed at its first rung must not wedge the host out of ever
    /// being restarted again.
    pub(crate) fn finish(&mut self, host: &str) {
        self.0.remove(host);
    }

    pub(crate) fn contains(&self, host: &str) -> bool {
        self.0.contains(host)
    }
}

/// One rung of a restart, in the order they run.
///
/// [`Relaunch`](RestartStep::Relaunch) is the plan's "spawn, then
/// reconnect" as one step on purpose: an explicit Connect already
/// spawns a missing localhost session through the shared ladder
/// (`ConnectMode::SpawnIfMissing`), so splitting it here would mean a
/// second copy of the ladder and two processes racing for the socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestartStep {
    /// `session.stop` — the session reaps its shells and answers.
    Stop,
    /// Wait for the socket to actually go. Spawning over a session that
    /// is still unbinding races the ladder's own `already-running`
    /// verdict.
    AwaitGone,
    /// Spawn if needed, then connect. The app's own Connect entry.
    Relaunch,
}

impl RestartStep {
    /// Every step, in order. The sequence itself is the contract this
    /// module exists to keep.
    pub(crate) const ORDER: [RestartStep; 3] = [Self::Stop, Self::AwaitGone, Self::Relaunch];

    /// How a failure at this step reads in the status bar.
    fn failed(self) -> &'static str {
        match self {
            Self::Stop => "could not stop the session",
            Self::AwaitGone => "the session did not finish stopping",
            Self::Relaunch => "could not start the session again",
        }
    }
}

/// A restart that did not finish, and how far it got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RestartFailure {
    pub(crate) step: RestartStep,
    pub(crate) message: String,
}

impl std::fmt::Display for RestartFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.step.failed(), self.message)
    }
}

/// Run the two rungs a restart owes before the connection set can dial
/// again: stop, then wait for the socket to go.
///
/// `Ok(())` means nothing is listening at `socket` any more — either
/// because this stopped it or because it was already gone, which is the
/// same state and the same success. The caller then performs
/// [`RestartStep::Relaunch`] through its ordinary Connect entry.
pub(crate) async fn stop_and_wait(socket: &Path) -> Result<(), RestartFailure> {
    let scale = session_launch::timeout_scale();
    let report = session_launch::stop_session(
        socket,
        session_launch::DEFAULT_STOP_CALL_BUDGET.mul_f64(scale),
    )
    .await
    .map_err(|error| RestartFailure {
        step: RestartStep::Stop,
        message: format!("{error:#}"),
    })?;
    // Nothing was listening: there is no session id to wait for leaving,
    // and no wait to spend. Restarting a session that is already down is
    // just starting one.
    let Some(report) = report else {
        return Ok(());
    };
    session_launch::await_stopped(
        socket,
        &report.identity.session_id,
        session_launch::DEFAULT_STOP_GONE_BUDGET.mul_f64(scale),
    )
    .await
    .map_err(|error| RestartFailure {
        step: RestartStep::AwaitGone,
        message: format!("{error:#}"),
    })
}

/// The owned form the app hands to an engine op — the future must
/// outlive the borrow of whatever resolved the target.
pub(crate) async fn stop_and_wait_owned(socket: PathBuf) -> Result<(), String> {
    stop_and_wait(&socket).await.map_err(|failure| {
        tracing::warn!(socket = %socket.display(), %failure, "host restart failed");
        failure.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The composition's order, pinned. Reordering these is a behavior
    /// change (spawning before the socket is unlinked loses the race to
    /// the outgoing session), so it is stated once and asserted.
    #[test]
    fn the_steps_run_stop_then_wait_then_relaunch() {
        assert_eq!(
            RestartStep::ORDER,
            [
                RestartStep::Stop,
                RestartStep::AwaitGone,
                RestartStep::Relaunch
            ]
        );
    }

    /// Every step names its own failure, and no two say the same thing —
    /// the whole reason the failure carries a step.
    #[test]
    fn each_step_fails_in_its_own_words() {
        let mut seen = Vec::new();
        for step in RestartStep::ORDER {
            let failure = RestartFailure {
                step,
                message: "connection refused".into(),
            };
            let rendered = failure.to_string();
            assert!(rendered.ends_with("connection refused"), "{rendered}");
            assert!(!seen.contains(&step.failed()), "{step:?} repeats a message");
            seen.push(step.failed());
        }
    }

    /// The whole point of claiming: the second press of "Restart
    /// session" is a no-op while the first ladder is running, because
    /// two stop+spawn ladders on one socket race — the second stop can
    /// reap the session the first one's relaunch just spawned.
    ///
    /// And the claim is per host, so two hosts restarting at once do not
    /// block each other.
    #[test]
    fn a_second_restart_is_refused_until_the_first_finishes() {
        let mut in_flight = RestartsInFlight::default();
        assert!(in_flight.begin("h1"), "the first press claims the ladder");
        assert!(!in_flight.begin("h1"), "the second press is a no-op");
        assert!(in_flight.contains("h1"));
        assert!(in_flight.begin("h2"), "another host is unaffected");

        in_flight.finish("h1");
        assert!(!in_flight.contains("h1"));
        assert!(
            in_flight.begin("h1"),
            "and a finished ladder can be run again — a failed stop must \
             not wedge the host out of restarting"
        );
        assert!(in_flight.contains("h2"), "which never touched the other");
    }

    /// A restart of a session that is already down succeeds without a
    /// daemon and without spending either budget — the path a user takes
    /// when the mismatched session has since exited on its own.
    #[tokio::test]
    async fn a_session_that_is_already_gone_needs_no_stopping() {
        let dir = tempfile::tempdir().expect("temp dir");
        stop_and_wait(&dir.path().join("absent.sock"))
            .await
            .expect("nothing to stop is not a failure");
    }

    /// A socket that is bound but serves nothing fails at
    /// [`RestartStep::Stop`] — and says so, rather than reporting a
    /// relaunch that never ran. The listener hangs up on every dial, so
    /// this is the wire's fastest "there is something here, and it is
    /// not a session".
    #[tokio::test]
    async fn a_socket_that_will_not_answer_fails_at_the_stop() {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("mute.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind");
        let accepting = tokio::spawn(async move {
            // Accept and drop: the probe takes one, the dial takes the
            // next, and the identify that follows reads EOF at once.
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });

        let failure = stop_and_wait(&socket)
            .await
            .expect_err("a socket nobody serves cannot be stopped");
        accepting.abort();

        assert_eq!(failure.step, RestartStep::Stop);
        assert!(
            failure
                .to_string()
                .starts_with("could not stop the session"),
            "{failure}"
        );
    }
}
