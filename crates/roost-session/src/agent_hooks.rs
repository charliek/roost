//! The host half of `session.set_agent_hooks` (plan 046 §3.4, reshaped
//! by plan 064 §3.3).
//!
//! `roost-engine` decodes the op; the work lands here, in the one
//! process that has both the install engine linked and the `$HOME` being
//! written. The engine never depends on `roost-agent-install` — it is
//! linked into the UI processes too, and a UI has no business carrying a
//! dotfile writer.
//!
//! **A client may only ever raise the host's `agent-hooks` key, never
//! lower it.** The wire carries an allow-list, and this module unions it
//! into whatever the key already says — `off`, unanswered, or a
//! narrower list — and wires what the union now allows. There is no way
//! to spell "off" or "narrow this" on this op: a client whose own
//! `agent-hooks` is `off`, or unconfigured, has nothing to widen the
//! host with, so it sends this op **not at all** (the caller's decision,
//! not this module's — see [`crate::agent_hooks`]'s sibling in
//! `roost-iced`). Taking entries back out is `roostctl agent
//! ensure`/`uninstall`, run by hand on the host itself.
//!
//! The entries themselves name no path — `installed_command` is
//! env-indirected through `$ROOST_AGENT_HOOK`, which
//! [`crate::agent_hook::hook_binary`] pins at `start` and every spawned
//! tab is handed. So wiring a host is machine-independent, and a session
//! that could not resolve its own binary wires entries that are inert
//! rather than wrong.
//!
//! **Two clients that raise different lists both win, additively.** The
//! record stores `by` (the asking client's label) and `wired_at`, and
//! each run is logged, so which client asked for which agent is
//! diagnosable from `roostctl agent status` on the host.

use roost_agent_install::{Guard, Home};
use roost_engine::ipc::{AgentHooksError, AgentHooksHandle, AgentHooksRequest};
use roost_ipc::messages::{AgentHooksFailed, AgentHooksOutcome, AgentHooksSkipped};
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

/// One raise, against an explicit [`Home`] — the seam the tests drive.
///
/// Only a whole-run install failure (no `$HOME`, an unwritable record, a
/// lock another writer held past the deadline) becomes an `Err` here, and
/// the engine turns that into one error frame. A *per-agent* failure is not
/// that: it rides back in [`AgentHooksOutcome::errors`], because
/// a codex file Roost could not parse must not cost the client the
/// session it just attached to.
///
/// A name this session does not recognise is reported as a skip rather
/// than refused here — validating the request (empty `agents`, or an
/// unrecognised name, as `invalid-param`) is the engine's job before this
/// ever runs (plan 064 C4), not this module's.
fn ensure_in(
    home: &Home,
    request: &AgentHooksRequest,
    guard: Guard,
) -> Result<AgentHooksOutcome, AgentHooksError> {
    let (agents, unknown) =
        roost_agent_install::resolve_names(request.agents.iter().map(String::as_str));
    let outcome = roost_agent_install::raise(home, &agents, &request.client, guard)
        .map_err(|error| AgentHooksError::Failed(error.to_string()))?;

    info!(
        client = %request.client,
        agents = ?request.agents,
        wired = outcome.wired.len(),
        refreshed = outcome.refreshed.len(),
        removed = outcome.removed.len(),
        errors = outcome.errors.len(),
        "a client raised this host's agent hooks"
    );
    for error in &outcome.errors {
        warn!(agent = error.agent.source(), %error.error, "agent hooks");
    }

    Ok(reply(&outcome, &unknown))
}

fn reply(outcome: &roost_agent_install::Outcome, unknown_names: &[String]) -> AgentHooksOutcome {
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
    skipped.extend(unknown_names.iter().map(|name| AgentHooksSkipped {
        agent: name.clone(),
        reason: format!(
            "no agent named that ({})",
            roost_agent_install::agent_names()
        ),
    }));
    AgentHooksOutcome {
        // The agents this host has wired and never announced — not the
        // ones this run happened to write. See the field's own doc.
        wired: names(&outcome.unnoticed),
        refreshed: names(&outcome.refreshed),
        // A raise never removes — see this module's own doc.
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

    fn request(agents: &[&str]) -> AgentHooksRequest {
        AgentHooksRequest {
            agents: agents.iter().map(|s| (*s).to_string()).collect(),
            client: "charlie-mbp".into(),
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
    fn a_raise_wires_the_named_present_agents_and_says_who_asked() {
        let dir = tempfile::tempdir().unwrap();
        let home = a_home(dir.path());

        let first = ensure_in(&home, &request(&["claude"]), Guard::PERMITTED).expect("raise");
        assert_eq!(first.wired, vec!["claude".to_string()]);
        assert!(first.errors.is_empty(), "{first:?}");
        let reasons: Vec<(&str, &str)> = first
            .skipped
            .iter()
            .map(|s| (s.agent.as_str(), s.reason.as_str()))
            .collect();
        assert!(
            reasons.contains(&("cursor", "not allowed")),
            "cursor is present but never named: {reasons:?}"
        );
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

    /// The toast is a property of the host, not of the call: the session
    /// flips `noticed` for what it reports, so the next client to
    /// connect hears nothing.
    ///
    /// The flip happens inside the raise's own lock, which is what makes
    /// this true of two *overlapping* clients and not just two sequential
    /// ones — a flip taken afterwards, under a re-acquired lock, would
    /// let both read the same agent as unannounced.
    #[test]
    fn a_second_client_is_told_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let home = a_home(dir.path());
        let ask = request(&["claude"]);

        let first = ensure_in(&home, &ask, Guard::PERMITTED).expect("raise");
        assert!(first.wired.contains(&"claude".to_string()));
        let second = ensure_in(&home, &ask, Guard::PERMITTED).expect("raise again");
        assert!(second.wired.is_empty(), "{second:?}");
        assert!(second.refreshed.is_empty(), "{second:?}");
    }

    /// A second client that raises a *different* agent widens the host
    /// rather than replacing the first client's grant — the additive
    /// half of the raise rule, and the reason this op never takes
    /// anything out.
    #[test]
    fn a_second_client_widens_rather_than_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let home = a_home(dir.path());
        ensure_in(&home, &request(&["claude"]), Guard::PERMITTED).expect("first raise");

        let second =
            ensure_in(&home, &request(&["cursor"]), Guard::PERMITTED).expect("second raise");
        assert!(second.wired.contains(&"cursor".to_string()), "{second:?}");
        assert!(
            second.removed.is_empty(),
            "a raise never removes: {second:?}"
        );
        assert!(
            std::fs::read_to_string(dir.path().join(".claude/settings.json"))
                .unwrap()
                .contains("ROOST_AGENT_HOOK"),
            "the first client's grant survives the second's raise"
        );
    }

    /// A name no agent answers to is reported and otherwise ignored —
    /// never a refusal here, so a newer client's agent name cannot break
    /// an older host. (Whether the request should have been refused
    /// outright is the engine's call, made before this function ever
    /// runs — see this module's own doc.)
    #[test]
    fn an_unknown_agent_name_is_reported_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let home = a_home(dir.path());

        let done = ensure_in(&home, &request(&["claude", "gemini"]), Guard::PERMITTED)
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
    fn the_test_mode_fence_applies_to_a_remote_raise() {
        let dir = tempfile::tempdir().unwrap();
        let home = a_home(dir.path());
        let guard = Guard {
            test_mode: true,
            forced: false,
        };

        let refused = ensure_in(&home, &request(&["claude"]), guard)
            .expect_err("test mode must stop the install engine dead")
            .to_string();
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
    fn a_raise_waits_for_a_busy_lock_and_still_runs() {
        let dir = tempfile::tempdir().unwrap();
        let home = a_home(dir.path());
        let held = roost_agent_install::write::lock(&home.lock_path()).expect("take the lock");
        let releasing = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            drop(held);
        });

        let done = ensure_in(&home, &request(&["claude"]), Guard::PERMITTED)
            .expect("the lock frees well inside the deadline");
        releasing.join().unwrap();
        assert!(done.wired.contains(&"claude".to_string()), "{done:?}");
    }
}
