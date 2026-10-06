//! The right-click menu on a tab pill, a project row or a host band
//! (plan 073 D9).
//!
//! One model for every renderer — the macOS popup, the Linux overlay and
//! the `app.context_menu_*` test ops all list and run what [`entries`]
//! returns. A host's items are not decided here: they are
//! [`host_verbs::verbs`]' own rows for that host, so the menu can never
//! offer a verb the palette withholds, or word it differently.

use crate::host_verbs::{self, HostVerb, VerbItem};
use crate::keys::{ProjectKey, TabKey};

/// The row a menu was opened on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextTarget {
    Tab(TabKey),
    Project(ProjectKey),
    /// A host band, by its saved id: a host that has never connected has
    /// no live `HostId` to be named by.
    Host(String),
}

/// Everything a menu item can do. The wire names are [`Self::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextAction {
    RenameTab,
    NewTabHere,
    CopyTabPath,
    CloseTab,
    NewTab,
    RenameProject,
    CopyProjectPath,
    OpenProjectFolder,
    CloseProject,
    HostConnect,
    HostDisconnect,
    HostUpdateSession,
    HostInstallUpdate,
    HostRestartSession,
    HostStopSession,
}

impl ContextAction {
    pub const ALL: [Self; 15] = [
        Self::RenameTab,
        Self::NewTabHere,
        Self::CopyTabPath,
        Self::CloseTab,
        Self::NewTab,
        Self::RenameProject,
        Self::CopyProjectPath,
        Self::OpenProjectFolder,
        Self::CloseProject,
        Self::HostConnect,
        Self::HostDisconnect,
        Self::HostUpdateSession,
        Self::HostInstallUpdate,
        Self::HostRestartSession,
        Self::HostStopSession,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::RenameTab => "rename_tab",
            Self::NewTabHere => "new_tab_here",
            Self::CopyTabPath => "copy_tab_path",
            Self::CloseTab => "close_tab",
            Self::NewTab => "new_tab",
            Self::RenameProject => "rename_project",
            Self::CopyProjectPath => "copy_project_path",
            Self::OpenProjectFolder => "open_project_folder",
            Self::CloseProject => "close_project",
            Self::HostConnect => "host_connect",
            Self::HostDisconnect => "host_disconnect",
            Self::HostUpdateSession => "host_update_session",
            Self::HostInstallUpdate => "host_install_update",
            Self::HostRestartSession => "host_restart_session",
            Self::HostStopSession => "host_stop_session",
        }
    }

    pub fn from_wire(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.as_str() == name)
    }

    /// The host verb this item runs, addressed to `saved_id`. `None` for
    /// every item that is not a host's.
    pub fn host_verb(self, saved_id: &str) -> Option<HostVerb> {
        let saved_id = saved_id.to_string();
        Some(match self {
            Self::HostConnect => HostVerb::Connect(saved_id),
            Self::HostDisconnect => HostVerb::Disconnect(saved_id),
            Self::HostUpdateSession => HostVerb::Update(saved_id),
            Self::HostInstallUpdate => HostVerb::Install(saved_id),
            Self::HostRestartSession => HostVerb::Restart(saved_id),
            Self::HostStopSession => HostVerb::Stop(saved_id),
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextEntry {
    Item {
        action: ContextAction,
        label: String,
        enabled: bool,
    },
    Separator,
}

/// What the app knows about the row, gathered fresh for every menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextFacts<'a> {
    /// The saved host the row lives on; `None` for the in-process
    /// workspace.
    pub host: Option<&'a str>,
    /// `false` for a dimmed or offline host's rows, which offer only that
    /// host's verbs: nothing else can reach a session that is not there.
    pub interactive: bool,
    /// Whether the row's path is on this machine — the in-process
    /// workspace, or this machine's own session — so a file manager here
    /// can open it.
    pub on_this_machine: bool,
    pub cwd_known: bool,
}

const OPEN_FOLDER_LABEL: &str = if cfg!(target_os = "macos") {
    "Open in Finder"
} else {
    "Open in File Manager"
};

/// The menu for `target`. `verbs` is [`host_verbs::verbs`]' answer for
/// the palette's own inputs; only the rows addressed to this row's host
/// are kept.
pub fn entries(
    target: &ContextTarget,
    facts: &ContextFacts<'_>,
    verbs: &[VerbItem],
) -> Vec<ContextEntry> {
    let host_rows = || {
        facts
            .host
            .map(|saved_id| host_items(verbs, saved_id))
            .unwrap_or_default()
    };
    match target {
        ContextTarget::Host(saved_id) => host_items(verbs, saved_id),
        _ if !facts.interactive => host_rows(),
        ContextTarget::Tab(_) => vec![
            item(ContextAction::RenameTab, "Rename…", true),
            item(ContextAction::NewTabHere, "New Tab Here", true),
            item(ContextAction::CopyTabPath, "Copy Path", facts.cwd_known),
            ContextEntry::Separator,
            item(ContextAction::CloseTab, "Close Tab", true),
        ],
        ContextTarget::Project(_) => {
            let mut menu = vec![
                item(ContextAction::NewTab, "New Tab", true),
                item(ContextAction::RenameProject, "Rename…", true),
                item(ContextAction::CopyProjectPath, "Copy Path", facts.cwd_known),
            ];
            if facts.on_this_machine {
                menu.push(item(
                    ContextAction::OpenProjectFolder,
                    OPEN_FOLDER_LABEL,
                    facts.cwd_known,
                ));
            }
            menu.push(ContextEntry::Separator);
            menu.push(item(ContextAction::CloseProject, "Close Project…", true));
            let hosts = host_rows();
            if !hosts.is_empty() {
                menu.push(ContextEntry::Separator);
                menu.extend(hosts);
            }
            menu
        }
    }
}

/// Whether `action` is on `entries`, and if so whether it is enabled.
pub fn item_enabled(entries: &[ContextEntry], action: ContextAction) -> Option<bool> {
    entries.iter().find_map(|entry| match entry {
        ContextEntry::Item {
            action: listed,
            enabled,
            ..
        } if *listed == action => Some(*enabled),
        _ => None,
    })
}

fn item(action: ContextAction, label: &str, enabled: bool) -> ContextEntry {
    ContextEntry::Item {
        action,
        label: label.to_string(),
        enabled,
    }
}

/// `verbs`' rows for one saved host, under their own titles. Remove Host
/// is left out (plan 073 decision g): it has no confirm card, and a
/// right-click misfires more easily than a typed palette command.
fn host_items(verbs: &[VerbItem], saved_id: &str) -> Vec<ContextEntry> {
    verbs
        .iter()
        .filter_map(|row| {
            let verb = host_verbs::parse(&row.id)?;
            let action = ContextAction::ALL
                .into_iter()
                .find(|action| action.host_verb(saved_id).as_ref() == Some(&verb))?;
            Some(item(action, &row.title, true))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use roost_ipc::LocalBackendMode;

    use super::*;
    use crate::host_sidebar::{FidelityAction, HostTransportKind, LocalSlot, SectionState};
    use crate::host_verbs::{HostRow, SlotHistory, VerbPolicy};
    use crate::keys::HostId;

    const IN_PROCESS: LocalSlot<'static> = LocalSlot {
        mode: LocalBackendMode::InProcess,
        slot_saved_id: None,
    };

    fn local_facts() -> ContextFacts<'static> {
        ContextFacts {
            host: None,
            interactive: true,
            on_this_machine: true,
            cwd_known: true,
        }
    }

    fn host(saved_id: &'static str, state: SectionState) -> HostRow<'static> {
        HostRow {
            saved_id,
            label: saved_id,
            target: saved_id,
            state,
            transport: HostTransportKind::Ssh,
            fidelity: None,
            update: None,
        }
    }

    fn labels(entries: &[ContextEntry]) -> Vec<&str> {
        entries
            .iter()
            .map(|entry| match entry {
                ContextEntry::Item { label, .. } => label.as_str(),
                ContextEntry::Separator => "─",
            })
            .collect()
    }

    fn actions(entries: &[ContextEntry]) -> Vec<Option<ContextAction>> {
        entries
            .iter()
            .map(|entry| match entry {
                ContextEntry::Item { action, .. } => Some(*action),
                ContextEntry::Separator => None,
            })
            .collect()
    }

    /// `verbs`' own rows addressed to `saved_id`, Remove Host aside, as
    /// `(title, "host_<verb>")` — the oracle the menu's host items must
    /// equal. Read off the row ids' spelling rather than through
    /// `host_verbs::parse`, so it is not the mapping under test restated.
    fn verbs_rows_for(verbs: &[VerbItem], saved_id: &str) -> Vec<(String, String)> {
        verbs
            .iter()
            .filter_map(|verb| {
                let (word, id) = verb.id.strip_prefix("host:")?.split_once(':')?;
                (id == saved_id && word != "remove")
                    .then(|| (verb.title.clone(), format!("host_{word}")))
            })
            .collect()
    }

    /// The menu's items as `(label, action)`, spelled the way the row ids
    /// are: no `_session` suffix, and `host_install` for Install Update.
    fn host_rows(entries: &[ContextEntry]) -> Vec<(String, String)> {
        entries
            .iter()
            .filter_map(|entry| match entry {
                ContextEntry::Item { action, label, .. } => Some((
                    label.clone(),
                    action
                        .as_str()
                        .trim_end_matches("_session")
                        .replace("host_install_update", "host_install"),
                )),
                ContextEntry::Separator => None,
            })
            .collect()
    }

    #[test]
    fn a_tab_offers_rename_new_tab_here_copy_path_and_close() {
        let menu = entries(&ContextTarget::Tab(TabKey::local(7)), &local_facts(), &[]);
        assert_eq!(
            labels(&menu),
            ["Rename…", "New Tab Here", "Copy Path", "─", "Close Tab"]
        );
        assert_eq!(
            actions(&menu),
            [
                Some(ContextAction::RenameTab),
                Some(ContextAction::NewTabHere),
                Some(ContextAction::CopyTabPath),
                None,
                Some(ContextAction::CloseTab),
            ]
        );
        assert!(menu
            .iter()
            .all(|entry| !matches!(entry, ContextEntry::Item { enabled: false, .. })));
    }

    #[test]
    fn a_local_project_offers_its_folder_and_no_host_rows() {
        let menu = entries(
            &ContextTarget::Project(ProjectKey::local(3)),
            &local_facts(),
            &[],
        );
        assert_eq!(
            labels(&menu),
            [
                "New Tab",
                "Rename…",
                "Copy Path",
                OPEN_FOLDER_LABEL,
                "─",
                "Close Project…"
            ]
        );
        assert_eq!(
            item_enabled(&menu, ContextAction::OpenProjectFolder),
            Some(true)
        );
    }

    #[test]
    fn copy_path_is_disabled_only_when_no_cwd_is_known() {
        let facts = ContextFacts {
            cwd_known: false,
            ..local_facts()
        };
        let tab = entries(&ContextTarget::Tab(TabKey::local(7)), &facts, &[]);
        assert_eq!(item_enabled(&tab, ContextAction::CopyTabPath), Some(false));
        assert_eq!(item_enabled(&tab, ContextAction::CloseTab), Some(true));
        let project = entries(&ContextTarget::Project(ProjectKey::local(3)), &facts, &[]);
        assert_eq!(
            item_enabled(&project, ContextAction::CopyProjectPath),
            Some(false)
        );
        assert_eq!(
            item_enabled(&project, ContextAction::OpenProjectFolder),
            Some(false),
            "there is no folder to open either"
        );
        assert_eq!(item_enabled(&project, ContextAction::NewTab), Some(true));
    }

    #[test]
    fn a_remote_project_offers_no_folder_and_its_hosts_verbs_after_a_separator() {
        let hosts = [host("aa", SectionState::Connected)];
        let verbs = host_verbs::verbs(
            &hosts,
            &[],
            IN_PROCESS,
            VerbPolicy::current(),
            false,
            SlotHistory::NeverConnected,
        );
        let facts = ContextFacts {
            host: Some("aa"),
            interactive: true,
            on_this_machine: false,
            cwd_known: true,
        };
        let menu = entries(
            &ContextTarget::Project(ProjectKey::new(HostId::new(2), 3)),
            &facts,
            &verbs,
        );
        assert_eq!(item_enabled(&menu, ContextAction::OpenProjectFolder), None);
        assert_eq!(
            labels(&menu),
            [
                "New Tab",
                "Rename…",
                "Copy Path",
                "─",
                "Close Project…",
                "─",
                "Disconnect Host: aa",
                "Stop Session: aa"
            ]
        );
    }

    #[test]
    fn a_dimmed_hosts_rows_offer_only_its_verbs() {
        let hosts = [host("aa", SectionState::Disconnected)];
        let verbs = host_verbs::verbs(
            &hosts,
            &[],
            IN_PROCESS,
            VerbPolicy::current(),
            false,
            SlotHistory::NeverConnected,
        );
        let facts = ContextFacts {
            host: Some("aa"),
            interactive: false,
            on_this_machine: false,
            cwd_known: true,
        };
        for target in [
            ContextTarget::Project(ProjectKey::new(HostId::new(2), 3)),
            ContextTarget::Tab(TabKey::new(HostId::new(2), 9)),
        ] {
            let menu = entries(&target, &facts, &verbs);
            assert_eq!(labels(&menu), ["Connect Host: aa"], "{target:?}");
        }
    }

    #[test]
    fn every_action_round_trips_its_wire_name() {
        for action in ContextAction::ALL {
            assert_eq!(ContextAction::from_wire(action.as_str()), Some(action));
        }
        assert_eq!(ContextAction::from_wire("remove_host"), None);
        assert_eq!(ContextAction::from_wire("RenameTab"), None);
    }

    /// The model parity test (plan 073 D9): for the same inputs, a host's
    /// menu rows are exactly `verbs`' rows for that host, Remove Host
    /// aside — in `verbs`' order, under `verbs`' titles, each running the
    /// verb its row names. Two hosts in every case, so a filter that let
    /// the other host's rows through would show.
    #[test]
    fn host_rows_equal_the_palettes_verbs_minus_remove() {
        struct Case {
            name: &'static str,
            hosts: Vec<HostRow<'static>>,
            local: LocalSlot<'static>,
            policy: VerbPolicy,
            expected: &'static [&'static str],
        }
        let connected = host("aa", SectionState::Connected);
        let other = host("bb", SectionState::Connected);
        let with_fidelity = |fidelity| HostRow {
            fidelity: Some(fidelity),
            ..connected
        };
        let slot = HostRow {
            transport: HostTransportKind::Localhost,
            ..connected
        };
        let session = LocalSlot {
            mode: LocalBackendMode::Session,
            slot_saved_id: Some("aa"),
        };
        let withheld = VerbPolicy {
            localhost_surface: false,
        };
        let cases = [
            Case {
                name: "connected",
                hosts: vec![connected, other],
                local: IN_PROCESS,
                policy: VerbPolicy::current(),
                expected: &["Disconnect Host: aa", "Stop Session: aa"],
            },
            Case {
                name: "offline",
                hosts: vec![host("aa", SectionState::Disconnected), other],
                local: IN_PROCESS,
                policy: VerbPolicy::current(),
                expected: &["Connect Host: aa"],
            },
            Case {
                name: "fidelity update",
                hosts: vec![with_fidelity(FidelityAction::Update), other],
                local: IN_PROCESS,
                policy: VerbPolicy::current(),
                expected: &[
                    "Disconnect Host: aa",
                    "Stop Session: aa",
                    "Install Update: aa",
                ],
            },
            Case {
                name: "fidelity restart",
                hosts: vec![with_fidelity(FidelityAction::Restart), other],
                local: IN_PROCESS,
                policy: VerbPolicy::current(),
                expected: &[
                    "Disconnect Host: aa",
                    "Stop Session: aa",
                    "Restart session on aa",
                ],
            },
            Case {
                name: "fidelity manual",
                hosts: vec![with_fidelity(FidelityAction::Manual), other],
                local: IN_PROCESS,
                policy: VerbPolicy::current(),
                expected: &["Disconnect Host: aa", "Stop Session: aa"],
            },
            Case {
                name: "the local session slot",
                hosts: vec![slot, other],
                local: session,
                policy: VerbPolicy::current(),
                expected: &["Disconnect Host: aa", "Stop Session: aa"],
            },
            Case {
                name: "localhost unreachable",
                hosts: vec![slot, other],
                local: IN_PROCESS,
                policy: withheld,
                expected: &[],
            },
        ];
        for case in cases {
            let verbs = host_verbs::verbs(
                &case.hosts,
                &[],
                case.local,
                case.policy,
                false,
                SlotHistory::Connected,
            );
            let menu = entries(&ContextTarget::Host("aa".into()), &local_facts(), &verbs);
            assert_eq!(
                host_rows(&menu),
                verbs_rows_for(&verbs, "aa"),
                "{}",
                case.name
            );
            assert_eq!(labels(&menu), case.expected, "{}", case.name);
            assert!(
                menu.iter()
                    .all(|entry| matches!(entry, ContextEntry::Item { enabled: true, .. })),
                "{}: a host row is listed only when it applies",
                case.name
            );
        }
    }
}
