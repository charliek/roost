//! What this client has learned about the binaries a host's session
//! could be restarted or updated onto (plan 076 D4, D5).
//!
//! Every record belongs to one running session: it is keyed by saved
//! host and stamped with the registry target and the session id it was
//! learned against, and a lookup for anything else reads as not checked.
//! Facts about one session say nothing about the next.
//!
//! The one non-pure part is [`identify_localhost`], which runs the
//! localhost candidates' `identify` on the engine runtime.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use roost_ipc::session_launch::{self, BinOrigin};
use roost_ipc::session_version::BuildId;
use roost_ui_model::keys::HostId;
use roost_ui_model::session_update::{
    self, Candidate, Identified, InstallKnowledge, TargetKnowledge, TargetSource, Why,
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct Known {
    target: String,
    session_id: String,
    restart: TargetKnowledge,
    install: InstallKnowledge,
    /// The localhost identification this record is waiting on, if any.
    generation: Option<u64>,
    /// The connection whose facts last landed, so a resync of that same
    /// connection is told apart from a new connection to the session.
    incarnation: Option<HostId>,
}

#[derive(Debug, Default)]
pub(crate) struct UpdateKnowledge {
    hosts: HashMap<String, Known>,
}

impl UpdateKnowledge {
    fn current(&self, saved_id: &str, target: &str, session_id: &str) -> Option<&Known> {
        self.hosts
            .get(saved_id)
            .filter(|known| known.target == target && known.session_id == session_id)
    }

    /// What is known about this host's session, or not-checked for a
    /// session nothing was learned about.
    pub(crate) fn lookup(
        &self,
        saved_id: &str,
        target: &str,
        session_id: &str,
    ) -> (TargetKnowledge, InstallKnowledge) {
        self.current(saved_id, target, session_id).map_or(
            (TargetKnowledge::NotChecked, InstallKnowledge::NotChecked),
            |known| (known.restart.clone(), known.install.clone()),
        )
    }

    /// A connection to `session_id` landed. Drops whatever was known
    /// about a different session, and answers whether a localhost
    /// identification should start under `generation`.
    ///
    /// Localhost re-identifies on every new connection, the same session
    /// included: a binary swapped while the host was away (a deb upgrade,
    /// a new app bundle) shows up at the next connect (D4). A resync of
    /// the connection already landed changes nothing, and an ssh host's
    /// knowledge survives a reconnect to the same session (D5).
    pub(crate) fn connected(
        &mut self,
        saved_id: &str,
        target: &str,
        session_id: &str,
        incarnation: HostId,
        localhost: bool,
        generation: u64,
    ) -> bool {
        if let Some(known) = self
            .hosts
            .get_mut(saved_id)
            .filter(|known| known.target == target && known.session_id == session_id)
        {
            if known.incarnation == Some(incarnation) {
                return false;
            }
            known.incarnation = Some(incarnation);
            if !localhost {
                return false;
            }
            known.restart = TargetKnowledge::Checking;
            known.generation = Some(generation);
            return true;
        }
        let restart = if localhost {
            TargetKnowledge::Checking
        } else {
            TargetKnowledge::NotChecked
        };
        self.learned(
            saved_id,
            target,
            session_id,
            restart,
            InstallKnowledge::NotChecked,
        );
        if let Some(known) = self.hosts.get_mut(saved_id) {
            known.generation = localhost.then_some(generation);
            known.incarnation = Some(incarnation);
        }
        localhost
    }

    /// A localhost identification answered. Ignored unless it is the one
    /// this host's current record is waiting on.
    pub(crate) fn identified(
        &mut self,
        saved_id: &str,
        generation: u64,
        restart: TargetKnowledge,
    ) -> bool {
        let Some(known) = self.hosts.get_mut(saved_id) else {
            return false;
        };
        if known.generation != Some(generation) {
            return false;
        }
        known.generation = None;
        known.restart = restart;
        true
    }

    /// A probe a person started (or this client's own install) taught
    /// something about an ssh host's session.
    pub(crate) fn learned(
        &mut self,
        saved_id: &str,
        target: &str,
        session_id: &str,
        restart: TargetKnowledge,
        install: InstallKnowledge,
    ) {
        let incarnation = self
            .current(saved_id, target, session_id)
            .and_then(|known| known.incarnation);
        self.hosts.insert(
            saved_id.to_string(),
            Known {
                target: target.to_string(),
                session_id: session_id.to_string(),
                restart,
                install,
                generation: None,
                incarnation,
            },
        );
    }

    pub(crate) fn forget(&mut self, saved_id: &str) {
        self.hosts.remove(saved_id);
    }
}

/// A finished localhost identification, on its way back to the main
/// thread.
#[derive(Debug)]
pub(crate) struct TargetResolved {
    pub(crate) saved_id: String,
    pub(crate) generation: u64,
    pub(crate) restart: TargetKnowledge,
}

/// The process facts the localhost candidates are read from. Captured
/// on the main thread, so the task carries values rather than reading
/// globals at some later moment.
#[derive(Debug, Clone)]
pub(crate) struct LaunchFacts {
    pub(crate) bin_override: Option<OsString>,
    pub(crate) caller_exe: Option<PathBuf>,
    pub(crate) path: Option<OsString>,
}

impl LaunchFacts {
    pub(crate) fn from_env() -> Self {
        Self {
            bin_override: std::env::var_os(session_launch::BIN_ENV),
            caller_exe: std::env::current_exe().ok(),
            path: std::env::var_os("PATH"),
        }
    }
}

/// Identify a localhost session's restart candidates and choose (D4).
///
/// `ROOST_SESSION_BIN`, when set, is the only candidate, exactly as the
/// launch ladder treats it. Otherwise the ladder's own pick comes first
/// and the running session's executable second.
pub(crate) async fn identify_localhost(
    facts: LaunchFacts,
    exe_path: Option<String>,
    running: BuildId,
    client: BuildId,
    session_id: String,
    generation: u64,
) -> TargetKnowledge {
    let overridden = facts.bin_override.as_ref().is_some_and(|v| !v.is_empty());
    let located = session_launch::locate_session_binary(
        facts.bin_override.as_deref(),
        facts.caller_exe.as_deref(),
        facts.path.as_deref(),
    );
    let mut candidates = Vec::new();
    let mut located_canonical = None;
    match located {
        Ok(found) => {
            let source = match found.origin {
                BinOrigin::Env => TargetSource::Override,
                BinOrigin::Sibling => TargetSource::Bundled,
                BinOrigin::Path => TargetSource::Installed,
            };
            candidates.push(candidate(&found.path, source).await);
            located_canonical = tokio::fs::canonicalize(&found.path).await.ok();
        }
        Err(_) if overridden => return TargetKnowledge::NoneUsable(Why::Override),
        Err(_) => {}
    }
    if let Some(exe) = exe_path.filter(|_| !overridden) {
        // Usually the session runs the very binary the ladder found;
        // identifying it twice would cost a spawn and could not change
        // the pick.
        let same = located_canonical.is_some()
            && tokio::fs::canonicalize(&exe).await.ok() == located_canonical;
        if !same {
            candidates.push(candidate(Path::new(&exe), TargetSource::Running).await);
        }
    }
    session_update::target_knowledge(
        session_update::select_target(candidates, &running, &client),
        &session_id,
        generation,
    )
}

async fn candidate(path: &Path, source: TargetSource) -> Candidate {
    let identified = match tokio::fs::metadata(path).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Identified::Missing,
        // Something is there that this user cannot look at, or it is not
        // a file: present, but nothing a restart could run.
        Err(error) => {
            tracing::debug!(path = %path.display(), %error, "restart candidate unreadable");
            Identified::Unreadable
        }
        Ok(meta) if !meta.is_file() => Identified::Unreadable,
        Ok(_) => match roost_ipc::bootstrap::local_identity(path).await {
            Ok(identity) => Identified::Build(BuildId::from(&identity)),
            Err(error) => {
                tracing::debug!(path = %path.display(), %error, "restart candidate did not identify");
                Identified::Unreadable
            }
        },
    };
    Candidate {
        path: path.display().to_string(),
        source,
        identified,
    }
}

impl super::App {
    /// A connection's identify facts landed: forget what was known about
    /// another session, and on localhost identify the restart candidates
    /// in the background, once per connection (D4's lifecycle). An ssh host
    /// is never probed here (D5).
    pub(super) fn host_session_identified(&mut self, saved_id: &str, incarnation: HostId) {
        let Ok(host) = self.saved_host(saved_id) else {
            return;
        };
        let generation = self.take_engine_op_id();
        let Some(facts) = self.hosts.facts(saved_id) else {
            return;
        };
        let localhost = super::servicing::transport_kind(&host.target).localhost();
        if !self.update_knowledge.connected(
            saved_id,
            &host.target,
            &facts.session_id,
            incarnation,
            localhost,
            generation,
        ) {
            return;
        }
        let exe_path = facts.exe_path.clone();
        let running = facts.running.clone();
        let session_id = facts.session_id.clone();
        let launch = LaunchFacts::from_env();
        let client = super::bootstrap::client_build_id().clone();
        let saved_id = saved_id.to_string();
        let feed = self.feed_tx.clone();
        self.runtime_handle.spawn(async move {
            let restart =
                identify_localhost(launch, exe_path, running, client, session_id, generation).await;
            feed.send(crate::engine_feed::EngineFeed::RestartTarget(Box::new(
                TargetResolved {
                    saved_id,
                    generation,
                    restart,
                },
            )));
        });
    }

    pub(super) fn restart_target_resolved(&mut self, resolved: TargetResolved) {
        let TargetResolved {
            saved_id,
            generation,
            restart,
        } = resolved;
        if !self
            .update_knowledge
            .identified(&saved_id, generation, restart)
        {
            tracing::debug!(host = %saved_id, generation, "dropped a superseded restart-target answer");
            return;
        }
        self.refresh_action_facts(&saved_id);
    }

    /// A probe a person started answered about an ssh host (D5): the
    /// exec rung is both the restart candidate and what an install
    /// staged. Recorded only against a session this client is connected
    /// to, because that is what the knowledge is keyed by.
    pub(super) fn learn_from_probe(
        &mut self,
        saved_id: &str,
        outcome: &roost_ipc::bootstrap::ProbeOutcome,
    ) {
        if !matches!(
            self.hosts.state(saved_id),
            Some(crate::host_conn::HostConnState::Connected)
        ) {
            return;
        }
        let Ok(host) = self.saved_host(saved_id) else {
            return;
        };
        let generation = self.take_engine_op_id();
        let Some(facts) = self.hosts.facts(saved_id) else {
            return;
        };
        let (restart, install) = session_update::ssh_knowledge(
            outcome,
            &facts.running,
            super::bootstrap::client_build_id(),
            &facts.session_id,
            generation,
        );
        self.update_knowledge
            .learned(saved_id, &host.target, &facts.session_id, restart, install);
        self.refresh_action_facts(saved_id);
    }

    /// The update facts for one saved host, present whenever the
    /// session's identity is known: connected, or refused at the gate.
    pub(super) fn host_update_facts(
        &self,
        saved_id: &str,
        target: &str,
    ) -> Option<session_update::UpdateFacts> {
        use crate::host_conn::HostConnState;
        use session_update::Gate;

        let (running, gate, session_id) = match self.hosts.state(saved_id)? {
            HostConnState::Connected => {
                let facts = self.hosts.facts(saved_id)?;
                let gate = if facts.reduced_fidelity {
                    Gate::ReducedFidelity
                } else {
                    Gate::Ok
                };
                (&facts.running, gate, Some(facts.session_id.as_str()))
            }
            HostConnState::NeedsRestart(mismatch) => (
                &mismatch.running,
                Gate::Failed {
                    protocol_newer: mismatch.protocol_newer(),
                },
                None,
            ),
            _ => return None,
        };
        let (restart, install) = session_id.map_or(
            (TargetKnowledge::NotChecked, InstallKnowledge::NotChecked),
            |session_id| self.update_knowledge.lookup(saved_id, target, session_id),
        );
        Some(session_update::UpdateFacts::new(
            session_update::UpdateInputs {
                running,
                client: super::bootstrap::client_build_id(),
                gate,
                target: &restart,
                install: &install,
                transport: super::servicing::transport_kind(target),
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIRST: HostId = HostId::new(1);
    const SECOND: HostId = HostId::new(2);
    const THIRD: HostId = HostId::new(3);

    #[test]
    fn every_new_localhost_connection_identifies_and_a_resync_does_not() {
        let mut known = UpdateKnowledge::default();
        assert!(known.connected("h", "localhost", "s1", FIRST, true, 1));
        assert_eq!(
            known.lookup("h", "localhost", "s1").0,
            TargetKnowledge::Checking
        );
        // The same connection's facts landing again (a resync) starts
        // nothing.
        assert!(!known.connected("h", "localhost", "s1", FIRST, true, 2));

        assert!(!known.identified("h", 2, TargetKnowledge::NoneUsable(Why::Older)));
        assert!(known.identified("h", 1, TargetKnowledge::NoneUsable(Why::Older)));
        assert_eq!(
            known.lookup("h", "localhost", "s1").0,
            TargetKnowledge::NoneUsable(Why::Older)
        );

        // A new connection to the very same session identifies again,
        // so a binary swapped while the host was away is seen (D4).
        assert!(known.connected("h", "localhost", "s1", SECOND, true, 4));
        assert_eq!(
            known.lookup("h", "localhost", "s1").0,
            TargetKnowledge::Checking
        );
        assert!(!known.identified("h", 1, TargetKnowledge::NotChecked));
        assert!(known.identified("h", 4, TargetKnowledge::NoneUsable(Why::Unreadable)));
        assert_eq!(
            known.lookup("h", "localhost", "s1").0,
            TargetKnowledge::NoneUsable(Why::Unreadable)
        );

        // Another session id, or the same id behind another target, is
        // not what was learned about.
        assert_eq!(
            known.lookup("h", "localhost", "s2").0,
            TargetKnowledge::NotChecked
        );
        assert_eq!(
            known.lookup("h", "/tmp/other.sock", "s1").0,
            TargetKnowledge::NotChecked
        );
        assert!(known.connected("h", "localhost", "s2", THIRD, true, 3));
        // A late answer for the session that went is ignored.
        assert!(!known.identified("h", 1, TargetKnowledge::NotChecked));
    }

    #[test]
    fn an_ssh_session_is_never_identified_in_the_background() {
        let mut known = UpdateKnowledge::default();
        assert!(!known.connected("h", "ssh://box", "s1", FIRST, false, 1));
        assert_eq!(
            known.lookup("h", "ssh://box", "s1"),
            (TargetKnowledge::NotChecked, InstallKnowledge::NotChecked)
        );

        let staged = InstallKnowledge::Staged(BuildId::default());
        known.learned(
            "h",
            "ssh://box",
            "s1",
            TargetKnowledge::NoneUsable(Why::Unreadable),
            staged.clone(),
        );
        assert_eq!(known.lookup("h", "ssh://box", "s1").1, staged);
        // A resync, or a reconnect to the same session, keeps it; a
        // different session drops it.
        assert!(!known.connected("h", "ssh://box", "s1", FIRST, false, 2));
        assert!(!known.connected("h", "ssh://box", "s1", SECOND, false, 2));
        assert_eq!(known.lookup("h", "ssh://box", "s1").1, staged);
        known.connected("h", "ssh://box", "s2", THIRD, false, 3);
        assert_eq!(
            known.lookup("h", "ssh://box", "s2").1,
            InstallKnowledge::NotChecked
        );

        known.learned("h", "ssh://box", "s2", TargetKnowledge::NotChecked, staged);
        known.forget("h");
        assert_eq!(
            known.lookup("h", "ssh://box", "s2").1,
            InstallKnowledge::NotChecked
        );
    }

    fn running() -> BuildId {
        BuildId {
            version: "0.0.22".into(),
            protocol: roost_ipc::messages::SESSION_PROTOCOL_VERSION,
            ..BuildId::default()
        }
    }

    fn facts(bin_override: Option<&Path>, caller_exe: Option<&Path>) -> LaunchFacts {
        LaunchFacts {
            bin_override: bin_override.map(|p| p.as_os_str().to_owned()),
            caller_exe: caller_exe.map(Path::to_path_buf),
            path: Some(OsString::new()),
        }
    }

    #[tokio::test]
    async fn an_override_that_cannot_run_is_the_whole_answer() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("roost-session");
        let knowledge = identify_localhost(
            facts(Some(&missing), None),
            Some("/bin/sh".into()),
            running(),
            running(),
            "s".into(),
            1,
        )
        .await;
        assert_eq!(knowledge, TargetKnowledge::NoneUsable(Why::Override));
    }

    #[tokio::test]
    async fn nothing_to_find_anywhere_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let knowledge = identify_localhost(
            facts(None, Some(&dir.path().join("roost"))),
            Some(dir.path().join("gone").display().to_string()),
            running(),
            running(),
            "s".into(),
            1,
        )
        .await;
        assert_eq!(knowledge, TargetKnowledge::NoneUsable(Why::Missing));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_candidate_this_user_cannot_look_at_is_unreadable_not_missing() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        let exe = locked.join("roost-session");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root reads through a 000 directory; there is nothing to test.
        let root = std::fs::metadata(&exe).is_ok();
        let knowledge = identify_localhost(
            facts(None, Some(&dir.path().join("roost"))),
            Some(exe.display().to_string()),
            running(),
            running(),
            "s".into(),
            1,
        )
        .await;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        if root {
            return;
        }
        assert_eq!(knowledge, TargetKnowledge::NoneUsable(Why::Unreadable));
    }

    /// An empty `ROOST_SESSION_BIN` is unset, as `locate_session_binary`
    /// has always read it: the other candidates still count, and the
    /// ladder answers exactly what it answers with no override at all.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_empty_override_is_no_override_as_the_launch_ladder_reads_it() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let caller = dir.path().join("roost");
        let empty = std::ffi::OsString::new();
        assert_eq!(
            session_launch::locate_session_binary(Some(&empty), Some(&caller), Some(&empty))
                .unwrap_err()
                .to_string(),
            session_launch::locate_session_binary(None, Some(&caller), Some(&empty))
                .unwrap_err()
                .to_string(),
        );

        let line = serde_json::to_string(&roost_ipc::messages::SessionBinaryIdentity {
            app_version: "0.0.22".into(),
            session_protocol: roost_ipc::messages::SESSION_PROTOCOL_VERSION,
            libghostty_build: "g".into(),
            ..Default::default()
        })
        .unwrap();
        let running_exe = dir.path().join("running-session");
        std::fs::write(&running_exe, format!("#!/bin/sh\necho '{line}'\n")).unwrap();
        std::fs::set_permissions(&running_exe, std::fs::Permissions::from_mode(0o755)).unwrap();

        let knowledge = identify_localhost(
            LaunchFacts {
                bin_override: Some(empty.clone()),
                caller_exe: Some(caller),
                path: Some(empty),
            },
            Some(running_exe.display().to_string()),
            running(),
            running(),
            "s".into(),
            1,
        )
        .await;
        let TargetKnowledge::Found(target) = knowledge else {
            panic!("an empty override must not be the only candidate: {knowledge:?}");
        };
        assert_eq!(target.source, TargetSource::Running);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_sibling_that_will_not_identify_is_unreadable_and_exe_path_still_counts() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let sibling = dir.path().join("roost-session");
        std::fs::write(&sibling, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&sibling, std::fs::Permissions::from_mode(0o755)).unwrap();
        let line = serde_json::to_string(&roost_ipc::messages::SessionBinaryIdentity {
            app_version: "0.0.23".into(),
            session_protocol: roost_ipc::messages::SESSION_PROTOCOL_VERSION,
            libghostty_build: "g".into(),
            ..Default::default()
        })
        .unwrap();
        let running_exe = dir.path().join("running-session");
        std::fs::write(&running_exe, format!("#!/bin/sh\necho '{line}'\n")).unwrap();
        std::fs::set_permissions(&running_exe, std::fs::Permissions::from_mode(0o755)).unwrap();

        let caller = dir.path().join("roost");
        let only_sibling = identify_localhost(
            facts(None, Some(&caller)),
            None,
            running(),
            running(),
            "s".into(),
            1,
        )
        .await;
        assert_eq!(only_sibling, TargetKnowledge::NoneUsable(Why::Unreadable));

        let with_running = identify_localhost(
            facts(None, Some(&caller)),
            Some(running_exe.display().to_string()),
            running(),
            running(),
            "s".into(),
            7,
        )
        .await;
        let TargetKnowledge::Found(target) = with_running else {
            panic!("expected the running binary, got {with_running:?}");
        };
        assert_eq!(target.source, TargetSource::Running);
        assert_eq!(target.identity.version, "0.0.23");
        assert_eq!(target.generation, 7);
    }
}
