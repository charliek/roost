//! The host half of `session.set_agent_hooks` (plan 046 §3.4, reshaped
//! by plan 064 §3.3).
//!
//! `roost-engine` decodes the op; the work lands here, in the one
//! process that has both the install engine linked and the `$HOME` being
//! written. The engine never depends on `roost-agent-install` — it is
//! linked into the UI processes too, and a UI has no business carrying a
//! dotfile writer.
//!
//! **Every machine has one agent-hooks setting: the `agent-hooks` key in
//! its own `config.conf`** — the same key whether the machine is used at
//! a desk or dialled into as a host. There is no separate host stance and
//! no pin.
//!
//! **A connecting client may only ever raise that key, never lower it.**
//! The wire carries an allow-list, and this module unions it into
//! whatever the key already says, then wires what the union now allows.
//! There is no way to spell "off" or "narrow this" on this op: a client
//! whose own `agent-hooks` is `off`, or unconfigured, has nothing to
//! widen the host with, so it sends this op **not at all** (the caller's
//! decision, not this module's — see `roost-iced`'s `remote_request`).
//!
//! The contentious half, stated plainly: **a host whose key is
//! explicitly `off` is raised too.** `off`, unanswered and a narrower
//! list are one case here — a host has no screen to ask on, so the client
//! in front of the user is the only authority there is, and the same-UID
//! socket is the consent boundary. Lowering a host is done *on that box*
//! (`roostctl agent ensure`/`uninstall`, the dialog, or editing the key),
//! and it holds until a more permissive client connects again.
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
            // Validated here as well as inside `ensure_in`, and the
            // order is the point: a malformed request is the client's
            // bug whatever state this machine is in, so a host without
            // a `$HOME` must still answer an empty `agents` with
            // `invalid-param` rather than `internal` and send the
            // client looking for the fault at this end. `resolve` is
            // pure, so the second call costs five string compares.
            resolve(&request.agents)?;
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
/// Validation happens here, at the top, rather than in the engine that
/// decodes the op: the agent set lives in the install engine, and this is
/// the only path into it, so a second caller of the handle cannot reach a
/// write without passing through [`resolve`] first.
fn ensure_in(
    home: &Home,
    request: &AgentHooksRequest,
    guard: Guard,
) -> Result<AgentHooksOutcome, AgentHooksError> {
    let agents = resolve(&request.agents)?;
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

    Ok(reply(&outcome))
}

/// The agents a request names, or the `invalid-param` it is refused with
/// before anything is written.
///
/// Both refusals are bugs in the client rather than states a host should
/// absorb. An empty list means the client had nothing to raise, and a
/// client with nothing to raise does not send the op at all. A name that
/// resolves to no agent cannot be a newer Roost talking to an older host
/// either: `SESSION_PROTOCOL_VERSION` is compared for **equality** at
/// attach, so both ends of this wire know the same five agents. Filtering
/// such a name out and reporting it as a skip would leave the client
/// believing it had raised something it had not.
fn resolve(names: &[String]) -> Result<Vec<roost_agent::Agent>, AgentHooksError> {
    // Checked before `resolve_names`, which skips blanks: skipping is
    // right for a human-typed CLI list and wrong here, where under
    // protocol equality an empty element can only be a client bug.
    if names.iter().any(|name| name.trim().is_empty()) {
        return Err(AgentHooksError::InvalidParam(
            "session.set_agent_hooks: `agents` carries an empty name".to_string(),
        ));
    }
    let (agents, unknown) = roost_agent_install::resolve_names(names.iter().map(String::as_str));
    if let Some(name) = unknown.first() {
        return Err(AgentHooksError::InvalidParam(format!(
            "session.set_agent_hooks: no agent named {name:?} ({})",
            roost_agent_install::agent_names()
        )));
    }
    if agents.is_empty() {
        return Err(AgentHooksError::InvalidParam(
            "session.set_agent_hooks requires a non-empty `agents`: a client with \
             nothing to raise does not send the op"
                .to_string(),
        ));
    }
    Ok(agents)
}

fn reply(outcome: &roost_agent_install::Outcome) -> AgentHooksOutcome {
    let names = |agents: &[roost_agent::Agent]| -> Vec<String> {
        agents.iter().map(|a| a.source().to_string()).collect()
    };
    let skipped: Vec<AgentHooksSkipped> = outcome
        .skipped
        .iter()
        .map(|skip| AgentHooksSkipped {
            agent: skip.agent.source().to_string(),
            reason: skip.reason.to_string(),
        })
        .collect();
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

    /// This home's `agent-hooks` value, read off the file rather than out
    /// of an outcome: the key is the durable half of a raise, and an
    /// outcome can be right while the write is not.
    fn key_of(home: &Home) -> String {
        let text = std::fs::read_to_string(home.config_path()).expect("config.conf");
        text.lines()
            .filter_map(|line| line.trim().strip_prefix("agent-hooks"))
            .filter_map(|rest| rest.trim().strip_prefix('='))
            .map(|value| value.trim().to_string())
            .next_back()
            .expect("an agent-hooks key")
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
        assert_eq!(key_of(&home), "claude, cursor");
        assert!(
            std::fs::read_to_string(dir.path().join(".claude/settings.json"))
                .unwrap()
                .contains("ROOST_AGENT_HOOK"),
            "the first client's grant survives the second's raise"
        );
    }

    /// The contentious half of §3.3: a host that said `off` is raised by
    /// a connecting client anyway. `off`, unanswered and a narrower list
    /// are one case on this path — the client in front of the user is the
    /// only authority a screenless host has.
    #[test]
    fn a_raise_widens_a_host_whose_key_is_explicitly_off() {
        let dir = tempfile::tempdir().unwrap();
        let home = a_home(dir.path());
        std::fs::create_dir_all(home.config_path().parent().unwrap()).unwrap();
        std::fs::write(home.config_path(), "agent-hooks = off\n").unwrap();

        let raised = ensure_in(&home, &request(&["claude"]), Guard::PERMITTED).expect("raise");
        assert!(raised.wired.contains(&"claude".to_string()), "{raised:?}");
        assert_eq!(key_of(&home), "claude");
        assert!(
            std::fs::read_to_string(dir.path().join(".claude/settings.json"))
                .unwrap()
                .contains("ROOST_AGENT_HOOK")
        );
    }

    fn refused(home: &Home, agents: &[&str]) -> String {
        match ensure_in(home, &request(agents), Guard::PERMITTED) {
            Err(AgentHooksError::InvalidParam(message)) => message,
            other => panic!("{agents:?} must be refused as invalid-param: {other:?}"),
        }
    }

    /// A name no agent answers to takes the whole request down, and the
    /// refusal names both the offender and what would have been accepted.
    /// The `claude` beside it is the point: a partly-valid list is not
    /// partly applied.
    #[test]
    fn an_unknown_agent_name_refuses_the_whole_request() {
        let dir = tempfile::tempdir().unwrap();
        let home = a_home(dir.path());

        let message = refused(&home, &["claude", "gemini"]);
        assert!(message.contains("gemini"), "{message}");
        for known in ["claude", "codex", "grok", "cursor", "opencode"] {
            assert!(message.contains(known), "{message}");
        }
        assert!(
            !dir.path().join(".claude/settings.json").exists(),
            "a refused request wired claude anyway"
        );
        assert!(
            !home.config_path().exists(),
            "a refused request wrote the key"
        );
    }

    /// An empty list is refused rather than absorbed as a no-op: a client
    /// with nothing to raise does not send the op, so an empty one is a
    /// bug the host has to say out loud. A blank *element* is the same
    /// bug in a different spelling, and is named separately so the
    /// client can tell "you sent me nothing" from "one of these is not a
    /// name".
    #[test]
    fn an_empty_agent_list_is_refused_before_anything_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let home = a_home(dir.path());

        assert!(refused(&home, &[]).contains("non-empty"));
        assert!(refused(&home, &["  "]).contains("empty name"));
        assert!(refused(&home, &["claude", ""]).contains("empty name"));
        assert!(
            !home.config_path().exists(),
            "a refused request wrote the key"
        );
        assert!(!dir.path().join(".config/roost/agent-hooks.json").exists());
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
    /// `roost_ui_model::config`'s to prove
    /// (`a_lock_nobody_releases_is_refused_at_the_deadline`); what
    /// matters here is that this path goes through that lock at all, so
    /// a session's mutation barrier — and with it `session.stop` — is
    /// released in bounded time whatever the home is mounted on.
    #[test]
    fn a_raise_waits_for_a_busy_lock_and_still_runs() {
        let dir = tempfile::tempdir().unwrap();
        let home = a_home(dir.path());
        let held = home.config_lock().expect("take the lock");
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
