//! Where this UI's own tabs run, what it publishes about that, and how
//! a launch decides between the two (plan 063 §D1/§D5/§D7).
//!
//! The [`LocalRoute`] snapshot is the only thing the engine's IPC
//! handler knows about the backend — it answers `identify` from it, and
//! nothing else in the UI may write it. [`App::publish_local_route`]
//! is the single writer; this module owns what it writes.
//!
//! [`App::publish_local_route`]: super::App::publish_local_route

use roost_ipc::messages::Project;
use roost_ipc::{LocalBackendMode, LocalRoute};
use roost_ui_model::config::RoostConfig;

/// What the effective-mode ladder concluded (plan 063 §D5 steps 2–4).
///
/// Three outcomes rather than a bare mode, because the fresh-install one
/// owes a config write and the other two do not — and because a
/// `Configured(InProcess)` and an `Existing` are the same backend for
/// very different reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModeLadder {
    /// The key was there. An unparseable value counts: it is
    /// `in-process` *configured badly*, which is still configured, and
    /// so must never trigger the fresh-install write (`config.rs`'s
    /// `local_backend_key_present`).
    Configured(LocalBackendMode),
    /// Nothing of this app is on disk: no layout, no config. A first run
    /// starts on a session, and the key is written so the next launch is
    /// `Configured` rather than deciding again.
    FreshInstall,
    /// A setup that predates the key. It keeps the backend it has always
    /// had; flipping it is a post-release follow-up, not a launch's
    /// decision to make.
    Existing,
}

impl ModeLadder {
    /// The backend this conclusion asks for, before any write has been
    /// attempted — see [`settled_mode`].
    pub(crate) fn mode(self) -> LocalBackendMode {
        match self {
            Self::Configured(mode) => mode,
            Self::FreshInstall => LocalBackendMode::Session,
            Self::Existing => LocalBackendMode::InProcess,
        }
    }
}

/// The ladder's first input: the configured backend, or `None` when the
/// key was never written.
///
/// **Presence, not value.** `local-backend = wat` parses to
/// `in-process` *and* records the key as present, and the difference
/// matters exactly once: a fresh-install write would overwrite the line
/// the user wrote, which is why AC1 pins "malformed → in-process, no
/// fresh write" as its own clause.
pub(crate) fn configured_key(config: &RoostConfig) -> Option<LocalBackendMode> {
    config
        .local_backend_key_present
        .then(|| LocalBackendMode::from(config.local_backend))
}

/// Plan 063 §D5's ladder, steps 2–4 (step 1, the switch journal, is a
/// later commit's).
///
/// `config_exists` is a separate question from `state_json_exists` and
/// closes the shared-config hazard: `config::default_path()` has **no
/// profile component** while `BundleProfile::state_json_path()` does, so
/// a first run under the dev profile finds an empty state dir beside the
/// developer's real `config.conf`. That is not a fresh install, and
/// treating it as one would write `local-backend = session` into the
/// config the *release* profile reads.
pub(crate) fn ladder(
    key: Option<LocalBackendMode>,
    state_json_exists: bool,
    config_exists: bool,
) -> ModeLadder {
    match key {
        Some(mode) => ModeLadder::Configured(mode),
        None if !state_json_exists && !config_exists => ModeLadder::FreshInstall,
        None => ModeLadder::Existing,
    }
}

/// The mode the launch actually runs on, once the fresh-install write
/// has been attempted.
///
/// A write that failed degrades to `in-process` **for this launch
/// only**: coming up on a session whose mode nothing recorded would mean
/// the next launch finds a populated `state.json`, concludes `Existing`,
/// and leaves that session's projects invisible. Next launch reads
/// whatever is genuinely on disk and decides again.
///
/// "Failed" includes *a config now exists* — the write is create-only
/// (`config::create_with_key`), so someone else winning the race is an
/// `AlreadyExists` and this launch defers to the file they wrote rather
/// than to a decision it can no longer record.
pub(crate) fn settled_mode(ladder: ModeLadder, fresh_write_ok: bool) -> LocalBackendMode {
    match ladder {
        ModeLadder::FreshInstall if !fresh_write_ok => LocalBackendMode::InProcess,
        ladder => ladder.mode(),
    }
}

/// The label the implicit slot host is saved under (plan 063 §D7).
///
/// `localhost`, else `localhost (2)`, `localhost (3)`… — an SSH host may
/// already hold the plain name, and `Workspace::add_host` enforces
/// unique labels. `accepts` is the registry's own check rather than a
/// comparison here: that rule is Unicode case folding in both
/// directions plus the reserved word `local`, and a second spelling of
/// it would drift. The counter starts at 2, so no candidate is ever the
/// reserved word.
///
/// `None` when the registry refuses every candidate, which means
/// something other than a name collision is wrong; the caller logs and
/// comes up with no slot rather than looping.
pub(crate) fn slot_label(accepts: impl Fn(&str) -> bool) -> Option<String> {
    const SUFFIX_LIMIT: u32 = 64;
    (1..=SUFFIX_LIMIT).find_map(|n| {
        let candidate = match n {
            1 => roost_ui_model::host_verbs::SEED_LABEL.to_string(),
            n => format!("{} ({n})", roost_ui_model::host_verbs::SEED_LABEL),
        };
        accepts(&candidate).then_some(candidate)
    })
}

/// What a launch under `session` owes the slot's first mirror (plan 063
/// §D5's "initial selection").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InitialSelection {
    /// Nothing left to do: the mode is not `session`, or a selection is
    /// already held and an unrelated one is preserved.
    Settled,
    /// The slot has published no rows to select yet. A fresh session
    /// seeds one; a session started empty gets one when it is seeded.
    Wait,
    /// Select this project/tab pair and attach to the tab.
    Select { project: i64, tab: i64 },
}

/// Decide it (plan 063 §D5).
///
/// `slot` is the slot's live rows and its own active tab id, and is
/// `None` until the slot is connected and interactive. The acceptance
/// criterion is a *visible, attached* tab, not "≥1 project", which is
/// why this resolves to a tab rather than to a project.
pub(crate) fn initial_selection(
    mode: LocalBackendMode,
    selection_held: bool,
    slot: Option<(&[Project], i64)>,
) -> InitialSelection {
    if mode != LocalBackendMode::Session || selection_held {
        return InitialSelection::Settled;
    }
    let Some((projects, active_tab_id)) = slot else {
        return InitialSelection::Wait;
    };
    // The session's own active tab wins, so a relaunch lands where the
    // last client left it; its project is found by holding that tab
    // rather than by position, because the session's project order is
    // not this client's.
    let project = projects
        .iter()
        .find(|project| project.tabs.iter().any(|tab| tab.id == active_tab_id))
        .or_else(|| projects.iter().find(|project| !project.tabs.is_empty()));
    let Some(project) = project else {
        return InitialSelection::Wait;
    };
    let tab = project
        .tabs
        .iter()
        .find(|tab| tab.id == active_tab_id)
        .or_else(|| project.tabs.first());
    match tab {
        Some(tab) => InitialSelection::Select {
            project: project.id,
            tab: tab.id,
        },
        None => InitialSelection::Wait,
    }
}

/// The snapshot for `mode`.
///
/// Both derived fields hang off the mode: in-process has no slot, so it
/// has neither a slot socket nor a slot selection. The socket comes
/// from the session bundle profile because that is what determines it —
/// the daemon binds that path whether or not anyone is connected to it.
pub(crate) fn route_snapshot(
    mode: LocalBackendMode,
    slot_active: Option<(i64, i64)>,
) -> LocalRoute {
    match mode {
        LocalBackendMode::InProcess => LocalRoute {
            mode,
            slot_socket: None,
            slot_active: None,
        },
        LocalBackendMode::Session => LocalRoute {
            mode,
            slot_socket: roost_ipc::session_socket_path(),
            slot_active,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roost_ipc::paths::BundleProfile;

    #[test]
    fn the_session_snapshot_carries_the_profile_socket_and_the_slot_selection() {
        let route = route_snapshot(LocalBackendMode::Session, Some((4, 9)));
        assert_eq!(route.mode, LocalBackendMode::Session);
        assert_eq!(route.slot_active, Some((4, 9)));
        assert_eq!(
            route.slot_socket,
            Some(
                BundleProfile::session()
                    .unwrap()
                    .socket_path
                    .to_string_lossy()
                    .into_owned()
            )
        );
    }

    /// In-process has no slot at all, so a selection handed in from a
    /// previous session mode cannot survive the switch back.
    #[test]
    fn the_in_process_snapshot_has_no_slot() {
        let route = route_snapshot(LocalBackendMode::InProcess, Some((4, 9)));
        assert_eq!(route.mode, LocalBackendMode::InProcess);
        assert_eq!(route.slot_socket, None);
        assert_eq!(route.slot_active, None);
    }

    /// The ladder, branch by branch (plan 063 §D5 / AC1).
    ///
    /// The `Configured` cases are asserted on the *variant*, not on the
    /// mode: `Configured(InProcess)` and `Existing` name the same
    /// backend, and only the first of them means "the user said so".
    #[test]
    fn the_ladder_prefers_the_key_then_a_fresh_install_then_today() {
        use LocalBackendMode::{InProcess, Session};

        // A key wins whatever is on disk, in both directions.
        for state_json in [false, true] {
            for config in [false, true] {
                assert_eq!(
                    ladder(Some(Session), state_json, config),
                    ModeLadder::Configured(Session),
                    "state_json={state_json} config={config}"
                );
                assert_eq!(
                    ladder(Some(InProcess), state_json, config),
                    ModeLadder::Configured(InProcess),
                    "state_json={state_json} config={config}"
                );
            }
        }

        // No key, but something of this app is already on disk. Either
        // half is enough to say "not a first run".
        assert_eq!(ladder(None, true, false), ModeLadder::Existing);
        assert_eq!(ladder(None, false, true), ModeLadder::Existing);
        assert_eq!(ladder(None, true, true), ModeLadder::Existing);

        // Nothing at all.
        assert_eq!(ladder(None, false, false), ModeLadder::FreshInstall);

        assert_eq!(ModeLadder::FreshInstall.mode(), Session);
        assert_eq!(ModeLadder::Existing.mode(), InProcess);
    }

    /// An unparseable value is "configured", so it takes the existing
    /// backend and — the part that matters — never triggers the
    /// fresh-install write that would overwrite the line the user
    /// wrote (AC1's last clause).
    ///
    /// Driven from real config text through the same two fields the
    /// launch reads, because the trap is in the *wiring*: `wat` resolves
    /// to the default `in-process`, so anything deciding "configured?"
    /// from the value rather than from presence gets this backwards on
    /// an otherwise-empty disk.
    #[test]
    fn a_malformed_value_is_configured_rather_than_fresh() {
        let bad = RoostConfig::parse("local-backend = wat\n");
        assert_eq!(configured_key(&bad), Some(LocalBackendMode::InProcess));
        assert_eq!(
            ladder(configured_key(&bad), false, false),
            ModeLadder::Configured(LocalBackendMode::InProcess),
            "an empty disk plus a bad value is still not a fresh install"
        );

        // The same empty disk with no key at all is the case the write
        // belongs to, which is what makes the row above load-bearing.
        let absent = RoostConfig::parse("");
        assert_eq!(configured_key(&absent), None);
        assert_eq!(
            ladder(configured_key(&absent), false, false),
            ModeLadder::FreshInstall
        );

        // And a good value round-trips, both ways.
        assert_eq!(
            configured_key(&RoostConfig::parse("local-backend = session\n")),
            Some(LocalBackendMode::Session)
        );
        assert_eq!(
            configured_key(&RoostConfig::parse("local-backend = in-process\n")),
            Some(LocalBackendMode::InProcess)
        );
    }

    /// A fresh install that cannot record its choice runs the launch
    /// on the old backend rather than on an unrecorded new one.
    #[test]
    fn a_failed_fresh_install_write_degrades_to_in_process_for_this_launch() {
        assert_eq!(
            settled_mode(ModeLadder::FreshInstall, true),
            LocalBackendMode::Session
        );
        assert_eq!(
            settled_mode(ModeLadder::FreshInstall, false),
            LocalBackendMode::InProcess
        );
        // Every other conclusion writes nothing, so the write's outcome
        // cannot move it.
        for ladder in [
            ModeLadder::Configured(LocalBackendMode::Session),
            ModeLadder::Configured(LocalBackendMode::InProcess),
            ModeLadder::Existing,
        ] {
            assert_eq!(settled_mode(ladder, false), ladder.mode(), "{ladder:?}");
            assert_eq!(settled_mode(ladder, true), ladder.mode(), "{ladder:?}");
        }
    }

    /// The label rule (plan 063 §D7), driven through a registry check
    /// that behaves like the real one.
    #[test]
    fn the_slot_label_steps_past_a_name_an_ssh_host_already_holds() {
        let taken =
            |names: &'static [&'static str]| move |candidate: &str| !names.contains(&candidate);
        assert_eq!(slot_label(taken(&[])).as_deref(), Some("localhost"));
        assert_eq!(
            slot_label(taken(&["localhost"])).as_deref(),
            Some("localhost (2)")
        );
        assert_eq!(
            slot_label(taken(&["localhost", "localhost (2)"])).as_deref(),
            Some("localhost (3)")
        );
        // The counter starts at 2, so `local` — which the registry
        // reserves — is never a candidate.
        assert!(slot_label(|candidate| {
            assert_ne!(candidate, "local");
            candidate == "localhost (4)"
        })
        .is_some());
        // A registry refusing everything is a different problem; the
        // caller logs rather than looping.
        assert_eq!(slot_label(|_| false), None);
    }

    fn project(id: i64, tabs: &[i64]) -> Project {
        use roost_ipc::messages::{Tab, TabState};
        Project {
            id,
            name: format!("p{id}"),
            cwd: "/tmp".into(),
            position: 0,
            created_at: 0,
            tabs: tabs
                .iter()
                .enumerate()
                .map(|(index, tab)| Tab {
                    id: *tab,
                    project_id: id,
                    title: String::new(),
                    cwd: "/tmp".into(),
                    state: TabState::Idle,
                    has_notification: false,
                    is_active: false,
                    user_titled: false,
                    position: index as i32,
                    created_at: 0,
                    last_active: 0,
                    hook_active: false,
                    shell_state: Default::default(),
                    agent_lifecycle: Default::default(),
                    ownership: None,
                })
                .collect(),
        }
    }

    /// Plan 063 §D5's initial selection, and AC2's "attached, not merely
    /// present": the answer is a tab.
    #[test]
    fn the_launch_selection_follows_the_sessions_own_active_tab() {
        let rows = [project(1, &[10, 11]), project(2, &[20, 21])];

        // The session's active tab, wherever it lives.
        assert_eq!(
            initial_selection(LocalBackendMode::Session, false, Some((&rows, 21))),
            InitialSelection::Select {
                project: 2,
                tab: 21
            }
        );
        // No active tab of its own (or one that closed): the first row
        // with something in it.
        assert_eq!(
            initial_selection(LocalBackendMode::Session, false, Some((&rows, 0))),
            InitialSelection::Select {
                project: 1,
                tab: 10
            }
        );
        // A project with no tabs is not somewhere to land.
        let empty = [project(1, &[]), project(2, &[20])];
        assert_eq!(
            initial_selection(LocalBackendMode::Session, false, Some((&empty, 0))),
            InitialSelection::Select {
                project: 2,
                tab: 20
            }
        );
    }

    /// The three ways there is nothing to do — including the one that
    /// keeps waiting, which is what a session seeded after connect
    /// depends on.
    #[test]
    fn the_launch_selection_waits_for_the_slot_and_yields_to_a_held_one() {
        let rows = [project(1, &[10])];
        assert_eq!(
            initial_selection(LocalBackendMode::InProcess, false, Some((&rows, 10))),
            InitialSelection::Settled,
            "in-process never had a slot to select from"
        );
        assert_eq!(
            initial_selection(LocalBackendMode::Session, true, Some((&rows, 10))),
            InitialSelection::Settled,
            "an unrelated remote-host selection is preserved"
        );
        assert_eq!(
            initial_selection(LocalBackendMode::Session, false, None),
            InitialSelection::Wait,
            "the slot is not connected yet"
        );
        assert_eq!(
            initial_selection(LocalBackendMode::Session, false, Some((&[], 0))),
            InitialSelection::Wait,
            "connected but empty: the seed has not landed yet"
        );
    }
}
