//! The host half of `session.set_agent_hooks` (plan 046 §3.4).
//!
//! `roost-engine` decodes the op and gates it on the interactive lease;
//! the work lands here, in the one process that has both the install
//! engine linked and the `$HOME` being written. The engine never depends
//! on `roost-agent-install` — it is linked into the UI processes too, and
//! a UI has no business carrying a dotfile writer.
//!
//! **The client's config is the authority, and it is re-sent on every
//! connect.** `mode = off` therefore *removes* Roost's entries here,
//! where on the client's own machine the same key only opts out of
//! future wiring: a host has no `config.conf` of its own to consult, so
//! if `off` did nothing remotely there would be no way to take a host's
//! entries back out short of an ssh session. Off is off everywhere.
//!
//! The entries themselves name no path — `installed_command` is
//! env-indirected through `$ROOST_AGENT_HOOK`, which
//! [`crate::agent_hook::hook_binary`] pins at `start` and every spawned
//! tab is handed. So wiring a host is machine-independent, and a session
//! that could not resolve its own binary wires entries that are inert
//! rather than wrong.
//!
//! **Two clients that disagree flip the files on every reconnect.** Last
//! writer wins by design: the record stores `by` (the asking client's
//! label) and `wired_at`, and each run is logged, so the oscillation is
//! diagnosable from `roostctl agent status` on the host. Reconciling the
//! two is filed as future work (§9), not solved here.

use roost_agent_install::{Guard, Home, InstallError, Mode, Outcome};
use roost_engine::ipc::{AgentHooksError, AgentHooksHandle, AgentHooksRequest};
use roost_ipc::messages::{
    AgentHooksFailed, AgentHooksMode, AgentHooksSkipped, SessionSetAgentHooksResult,
};
use tracing::{info, warn};

/// The callback `IpcHandler::with_agent_hooks` takes.
///
/// `spawn_blocking` because the install reads and rewrites up to five
/// config files under an advisory `flock`, on a `$HOME` that may be
/// network-mounted — none of which belongs on a tokio worker that is
/// also serving this session's other connections.
pub fn handle() -> AgentHooksHandle {
    AgentHooksHandle::new(|request: AgentHooksRequest| async move {
        tokio::task::spawn_blocking(move || {
            let home =
                Home::from_env().map_err(|error| AgentHooksError::Failed(error.to_string()))?;
            ensure_in(&home, &request, Guard::from_env())
        })
        .await
        .map_err(|error| {
            AgentHooksError::Failed(format!("the agent-hooks install did not finish: {error}"))
        })?
    })
}

/// One ensure, against an explicit [`Home`] — the seam the tests drive.
///
/// Only an [`InstallError`] (no `$HOME`, an unwritable record, a lock
/// another writer held past the deadline) becomes an `Err` here, and the
/// engine turns that into one error frame. A *per-agent* failure is not
/// that: it rides back in [`SessionSetAgentHooksResult::errors`], because
/// a codex file Roost could not parse must not cost the client the
/// session it just attached to.
///
/// `ensure_on_behalf` rather than `ensure`, for the two things that are
/// only true remotely: the asking client's authority is re-checked once
/// the install holds its lock (the door check it passed can be seconds
/// stale by then — see `session_set_agent_hooks`), and the record's
/// `noticed` flag is flipped in that same locked write. The flip belongs
/// there because this reply *is* the announcement: there is no second
/// step to defer it to, and doing it afterwards under a re-taken lock let
/// two clients connecting at once both be told about the same agent.
fn ensure_in(
    home: &Home,
    request: &AgentHooksRequest,
    guard: Guard,
) -> Result<SessionSetAgentHooksResult, AgentHooksError> {
    let (skip, unknown) = roost_agent_install::skip_list(request.skip.iter().map(String::as_str));
    let mode = match request.mode {
        AgentHooksMode::Auto => Mode::Auto,
        AgentHooksMode::Off => Mode::Off,
    };
    let outcome =
        roost_agent_install::ensure_on_behalf(home, mode, &skip, &request.client, guard, &|| {
            request.authority.holds()
        })
        .map_err(|error| match error {
            InstallError::Unauthorized => AgentHooksError::Unauthorized,
            other => AgentHooksError::Failed(other.to_string()),
        })?;

    info!(
        client = %request.client,
        mode = ?request.mode,
        wired = outcome.wired.len(),
        refreshed = outcome.refreshed.len(),
        removed = outcome.removed.len(),
        errors = outcome.errors.len(),
        "a client set this host's agent hooks"
    );
    for error in &outcome.errors {
        warn!(agent = error.agent.source(), %error.error, "agent hooks");
    }

    Ok(reply(&outcome, &unknown))
}

fn reply(outcome: &Outcome, unknown_skip_names: &[String]) -> SessionSetAgentHooksResult {
    let names = |agents: &[roost_agent::Agent]| -> Vec<String> {
        agents.iter().map(|a| a.source().to_string()).collect()
    };
    let mut skipped: Vec<AgentHooksSkipped> = outcome
        .skipped
        .iter()
        .map(|skip| AgentHooksSkipped {
            agent: skip.agent.source().to_string(),
            reason: skip.reason.to_string(),
        })
        .collect();
    // Reported, never fatal: a name this session does not recognise is
    // most likely a typo, and refusing the whole run would turn it into
    // "nothing is wired and nothing says why". It may equally be an
    // agent a newer client knows about, which is the second reason not
    // to treat it as an error.
    skipped.extend(unknown_skip_names.iter().map(|name| AgentHooksSkipped {
        agent: name.clone(),
        reason: format!(
            "no agent named that ({})",
            roost_agent_install::agent_names()
        ),
    }));
    SessionSetAgentHooksResult {
        // The agents this host has wired and never announced — not the
        // ones this run happened to write. See the field's own doc.
        wired: names(&outcome.unnoticed),
        refreshed: names(&outcome.refreshed),
        removed: names(&outcome.removed),
        skipped,
        errors: outcome
            .errors
            .iter()
            .map(|failure| AgentHooksFailed {
                agent: failure.agent.source().to_string(),
                error: failure.error.to_string(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use roost_engine::ipc::AgentHooksAuthority;

    fn request(mode: AgentHooksMode, skip: &[&str]) -> AgentHooksRequest {
        with_authority(mode, skip, AgentHooksAuthority::always())
    }

    fn with_authority(
        mode: AgentHooksMode,
        skip: &[&str],
        authority: AgentHooksAuthority,
    ) -> AgentHooksRequest {
        AgentHooksRequest {
            mode,
            skip: skip.iter().map(|s| (*s).to_string()).collect(),
            client: "charlie-mbp".into(),
            authority,
        }
    }

    fn failure(error: AgentHooksError) -> String {
        match error {
            AgentHooksError::Failed(message) => message,
            other => panic!("expected a whole-run failure, got {other:?}"),
        }
    }

    /// A home with claude and cursor present and nothing else, so the
    /// three answers this reply distinguishes — wired, skipped by name,
    /// absent — all appear in one run.
    fn a_home(root: &std::path::Path) -> Home {
        for agent in [".claude", ".cursor"] {
            std::fs::create_dir_all(root.join(agent)).unwrap();
        }
        Home::rooted(root)
    }

    #[test]
    fn auto_wires_the_present_agents_and_says_who_asked() {
        let dir = tempfile::tempdir().unwrap();
        let home = a_home(dir.path());

        let first = ensure_in(
            &home,
            &request(AgentHooksMode::Auto, &["cursor"]),
            Guard::PERMITTED,
        )
        .expect("ensure");
        assert_eq!(first.wired, vec!["claude".to_string()]);
        assert!(first.errors.is_empty(), "{first:?}");
        let reasons: Vec<(&str, &str)> = first
            .skipped
            .iter()
            .map(|s| (s.agent.as_str(), s.reason.as_str()))
            .collect();
        assert!(reasons.contains(&("cursor", "skip-list")), "{reasons:?}");
        assert!(
            reasons.iter().any(|(agent, _)| *agent == "codex"),
            "an absent agent is a skip, not an error: {reasons:?}"
        );
        assert!(
            std::fs::read_to_string(dir.path().join(".claude/settings.json"))
                .unwrap()
                .contains("ROOST_AGENT_HOOK")
        );
        // `by` is what makes two clients of one host tellable apart.
        let record = std::fs::read_to_string(dir.path().join(".config/roost/agent-hooks.json"))
            .expect("state record");
        assert!(record.contains("charlie-mbp"), "{record}");
    }

    /// A request whose lease was taken over while it waited writes
    /// nothing at all — not the agents' files, and not the state record
    /// that says who wired them.
    ///
    /// The window is real: the install engine blocks on a per-home lock,
    /// and neither dropping the client's connection nor its own 15 s
    /// timeout cancels a handler already running. So an `auto` request
    /// stuck behind another writer could land *after* the client that
    /// displaced it had already told the host `off`.
    #[test]
    fn a_displaced_lease_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let home = a_home(dir.path());

        let lost = ensure_in(
            &home,
            &with_authority(
                AgentHooksMode::Auto,
                &[],
                AgentHooksAuthority::new(|| false),
            ),
            Guard::PERMITTED,
        )
        .expect_err("a request that lost the lease must not write");
        assert!(
            matches!(lost, AgentHooksError::Unauthorized),
            "and it says so as a takeover, not as an internal fault: {lost:?}"
        );
        assert!(!dir.path().join(".claude/settings.json").exists());
        assert!(!dir.path().join(".config/roost/agent-hooks.json").exists());
    }

    /// The toast is a property of the host, not of the call: the session
    /// flips `noticed` for what it reports, so the next client to
    /// connect hears nothing.
    ///
    /// The flip happens inside the ensure's own lock, which is what makes
    /// this true of two *overlapping* clients and not just two sequential
    /// ones — a flip taken afterwards, under a re-acquired lock, would
    /// let both read the same agent as unannounced.
    #[test]
    fn a_second_client_is_told_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let home = a_home(dir.path());
        let ask = request(AgentHooksMode::Auto, &[]);

        let first = ensure_in(&home, &ask, Guard::PERMITTED).expect("ensure");
        assert!(first.wired.contains(&"claude".to_string()));
        let second = ensure_in(&home, &ask, Guard::PERMITTED).expect("ensure again");
        assert!(second.wired.is_empty(), "{second:?}");
        assert!(second.refreshed.is_empty(), "{second:?}");
    }

    /// `off` from the client takes the host's entries back out — the
    /// half of the pin that makes an opt-out reach a machine that has no
    /// config of its own.
    #[test]
    fn off_from_the_client_unwires_the_host() {
        let dir = tempfile::tempdir().unwrap();
        let home = a_home(dir.path());
        ensure_in(&home, &request(AgentHooksMode::Auto, &[]), Guard::PERMITTED).expect("wire");

        let swept =
            ensure_in(&home, &request(AgentHooksMode::Off, &[]), Guard::PERMITTED).expect("unwire");
        assert!(swept.removed.contains(&"claude".to_string()), "{swept:?}");
        assert!(swept.wired.is_empty(), "{swept:?}");
        let settings =
            std::fs::read_to_string(dir.path().join(".claude/settings.json")).unwrap_or_default();
        assert!(!settings.contains("ROOST_AGENT_HOOK"), "{settings}");
    }

    /// A skip name no agent answers to is reported and otherwise
    /// ignored — never a refusal, so a newer client's agent name cannot
    /// break an older host.
    #[test]
    fn an_unknown_skip_name_is_reported_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let home = a_home(dir.path());

        let done = ensure_in(
            &home,
            &request(AgentHooksMode::Auto, &["gemini"]),
            Guard::PERMITTED,
        )
        .expect("an unknown name must not fail the run");
        assert!(done.wired.contains(&"claude".to_string()), "{done:?}");
        let named = done
            .skipped
            .iter()
            .find(|skip| skip.agent == "gemini")
            .expect("the unknown name is reported back to the client");
        assert!(named.reason.contains("no agent named that"), "{named:?}");
    }

    /// The harness fence reaches this path too: a session launched with
    /// `ROOST_TEST_MODE=1` and no explicit override writes nothing, and
    /// says so rather than reporting an empty success.
    #[test]
    fn the_test_mode_fence_applies_to_a_remote_ensure() {
        let dir = tempfile::tempdir().unwrap();
        let home = a_home(dir.path());
        let guard = Guard {
            test_mode: true,
            forced: false,
        };

        let refused = failure(
            ensure_in(&home, &request(AgentHooksMode::Auto, &[]), guard)
                .expect_err("test mode must stop the install engine dead"),
        );
        assert!(refused.contains("ROOST_TEST_MODE"), "{refused}");
        assert!(!dir.path().join(".claude/settings.json").exists());
    }

    /// A run whose lock is busy *waits* for it and then succeeds — the
    /// deadline is a backstop, not a fast failure.
    ///
    /// The bound itself, and the typed refusal at the end of it, are
    /// `roost_agent_install::write`'s to prove
    /// (`a_lock_nobody_releases_is_refused_at_the_deadline`); what
    /// matters here is that this path goes through that lock at all, so
    /// a session's mutation barrier — and with it `session.stop` — is
    /// released in bounded time whatever the home is mounted on.
    #[test]
    fn an_ensure_waits_for_a_busy_lock_and_still_runs() {
        let dir = tempfile::tempdir().unwrap();
        let home = a_home(dir.path());
        let held = roost_agent_install::write::lock(&home.lock_path()).expect("take the lock");
        let releasing = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            drop(held);
        });

        let done = ensure_in(&home, &request(AgentHooksMode::Auto, &[]), Guard::PERMITTED)
            .expect("the lock frees well inside the deadline");
        releasing.join().unwrap();
        assert!(done.wired.contains(&"claude".to_string()), "{done:?}");
    }
}
