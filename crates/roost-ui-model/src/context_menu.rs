//! The right-click menu on a tab pill, a project row or a host band
//! (plan 073 D9).
//!
//! One model for every renderer — the macOS popup, the Linux overlay and
//! the `app.context_menu_*` test ops all list and run what [`entries`]
//! returns. Which host items exist is not decided here: they are
//! [`host_verbs::verbs`]' own rows for that host that are marked for the
//! menu, so the two surfaces can never disagree about what a verb does.
//! Only their order, the header and the separators are the menu's
//! (plan 076 D6).

use crate::host_verbs::{self, HostVerb, VerbItem};
use crate::keys::{ProjectKey, TabKey};
use crate::session_update;

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
    /// Text only: never chosen, and the keys step past it.
    Header(String),
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
    /// The host's label, for the header a project's or tab's host block
    /// opens with.
    pub host_label: Option<&'a str>,
    /// [`crate::session_update::session_line`] for the host, once its
    /// session's identity is known.
    pub session_line: Option<&'a str>,
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
    // A row's host block: the host named in its header, since the
    // labels are short.
    let host_rows = || {
        let Some(saved_id) = facts.host else {
            return Vec::new();
        };
        let items = host_items(verbs, saved_id);
        if items.is_empty() {
            return items;
        }
        let label = facts.host_label.unwrap_or(saved_id);
        let mut block = vec![ContextEntry::Header(session_update::host_header(
            label,
            facts.session_line,
        ))];
        block.extend(items);
        block
    };
    match target {
        ContextTarget::Host(saved_id) => {
            let mut menu: Vec<ContextEntry> = facts
                .session_line
                .map(|line| ContextEntry::Header(line.to_string()))
                .into_iter()
                .collect();
            menu.extend(host_items(verbs, saved_id));
            menu
        }
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

/// The order a host's items are listed in (D6's band-menu table): the
/// way in, then what updates the session, then — after a separator —
/// the ways to leave it.
const HOST_ORDER: [ContextAction; 6] = [
    ContextAction::HostConnect,
    ContextAction::HostInstallUpdate,
    ContextAction::HostUpdateSession,
    ContextAction::HostRestartSession,
    ContextAction::HostDisconnect,
    ContextAction::HostStopSession,
];

/// Where the separator goes: before the first of these.
const LEAVING: [ContextAction; 2] = [
    ContextAction::HostDisconnect,
    ContextAction::HostStopSession,
];

/// `verbs`' menu rows for one saved host, under their menu labels, in
/// [`HOST_ORDER`]. Remove Host is never a menu row (plan 073 decision g):
/// it has no confirm card, and a right-click misfires more easily than a
/// typed palette command.
fn host_items(verbs: &[VerbItem], saved_id: &str) -> Vec<ContextEntry> {
    let mut rows: Vec<(ContextAction, &str)> = verbs
        .iter()
        .filter(|row| row.surfaces.menu)
        .filter_map(|row| {
            let verb = host_verbs::parse(&row.id)?;
            let action = HOST_ORDER
                .into_iter()
                .find(|action| action.host_verb(saved_id).as_ref() == Some(&verb))?;
            Some((action, row.menu_label))
        })
        .collect();
    rows.sort_by_key(|(action, _)| HOST_ORDER.iter().position(|listed| listed == action));
    let mut menu = Vec::with_capacity(rows.len() + 1);
    for (index, (action, label)) in rows.iter().enumerate() {
        if index > 0 && LEAVING.contains(action) && !LEAVING.contains(&rows[index - 1].0) {
            menu.push(ContextEntry::Separator);
        }
        menu.push(item(*action, label, true));
    }
    menu
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

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
            host_label: None,
            session_line: None,
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
                ContextEntry::Header(text) => text.as_str(),
            })
            .collect()
    }

    fn actions(entries: &[ContextEntry]) -> Vec<Option<ContextAction>> {
        entries
            .iter()
            .map(|entry| match entry {
                ContextEntry::Item { action, .. } => Some(*action),
                ContextEntry::Separator | ContextEntry::Header(_) => None,
            })
            .collect()
    }

    /// `verbs`' menu rows addressed to `saved_id`, Remove Host aside, as
    /// `(menu label, "host_<verb>")` — the oracle the menu's host items
    /// must equal as a set. Read off the row ids' spelling rather than
    /// through `host_verbs::parse`, so it is not the mapping under test
    /// restated.
    fn menu_rows_for(verbs: &[VerbItem], saved_id: &str) -> BTreeSet<(String, String)> {
        verbs
            .iter()
            .filter(|verb| verb.surfaces.menu)
            .filter_map(|verb| {
                let (word, id) = verb.id.strip_prefix("host:")?.split_once(':')?;
                (id == saved_id && word != "remove")
                    .then(|| (verb.menu_label.to_string(), format!("host_{word}")))
            })
            .collect()
    }

    /// The menu's items as `(label, action)`, spelled the way the row ids
    /// are: no `_session` suffix, and `host_install` for Install Update.
    fn host_rows(entries: &[ContextEntry]) -> BTreeSet<(String, String)> {
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
                ContextEntry::Separator | ContextEntry::Header(_) => None,
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
            host_label: Some("mini3"),
            session_line: Some("Session 0.0.21 · 0.0.22 available"),
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
                "mini3 · Session 0.0.21 · 0.0.22 available",
                "Disconnect",
                "Stop Session…"
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
            host_label: Some("box"),
            session_line: None,
        };
        for target in [
            ContextTarget::Project(ProjectKey::new(HostId::new(2), 3)),
            ContextTarget::Tab(TabKey::new(HostId::new(2), 9)),
        ] {
            let menu = entries(&target, &facts, &verbs);
            assert_eq!(labels(&menu), ["box", "Connect"], "{target:?}");
        }
        // The band itself names no host, and a host with no session
        // known has no line to open with.
        let band = entries(&ContextTarget::Host("aa".into()), &facts, &verbs);
        assert_eq!(labels(&band), ["Connect"]);
    }

    #[test]
    fn every_action_round_trips_its_wire_name() {
        for action in ContextAction::ALL {
            assert_eq!(ContextAction::from_wire(action.as_str()), Some(action));
        }
        assert_eq!(ContextAction::from_wire("remove_host"), None);
        assert_eq!(ContextAction::from_wire("RenameTab"), None);
    }

    fn update_facts(
        state: crate::session_update::SessionUpdate,
        offered: bool,
    ) -> crate::session_update::UpdateFacts {
        crate::session_update::UpdateFacts {
            state,
            running: Default::default(),
            client: Default::default(),
            restart: crate::session_update::RestartOffer {
                offered,
                why: None,
                target: None,
            },
            staged: None,
        }
    }

    /// The model parity test (plan 073 D9, rewritten for plan 076 D6):
    /// for the same inputs, a host's menu items are exactly `verbs`' rows
    /// for that host marked for the menu, Remove Host aside, under their
    /// menu labels, each running the verb its row names. Two hosts in
    /// every case, so a filter that let the other host's rows through
    /// would show. The order is [`HOST_ORDER`]'s, asserted separately.
    #[test]
    fn host_rows_equal_the_verbs_marked_for_the_menu_minus_remove() {
        use crate::session_update::SessionUpdate;
        struct Case {
            name: &'static str,
            hosts: Vec<HostRow<'static>>,
            local: LocalSlot<'static>,
            policy: VerbPolicy,
            expected: &'static [&'static str],
        }
        static AVAILABLE: std::sync::LazyLock<crate::session_update::UpdateFacts> =
            std::sync::LazyLock::new(|| update_facts(SessionUpdate::Available, true));
        static UP_TO_DATE: std::sync::LazyLock<crate::session_update::UpdateFacts> =
            std::sync::LazyLock::new(|| update_facts(SessionUpdate::UpToDate, true));
        static REQUIRED: std::sync::LazyLock<crate::session_update::UpdateFacts> =
            std::sync::LazyLock::new(|| update_facts(SessionUpdate::Required, true));
        // `restart_offer` never offers a refused ssh session a Restart.
        static REQUIRED_SSH: std::sync::LazyLock<crate::session_update::UpdateFacts> =
            std::sync::LazyLock::new(|| update_facts(SessionUpdate::Required, false));
        static BLOCKED: std::sync::LazyLock<crate::session_update::UpdateFacts> =
            std::sync::LazyLock::new(|| {
                update_facts(SessionUpdate::SessionNewer { blocked: true }, false)
            });
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
        let refused = host("aa", SectionState::NeedsRestart);
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
                expected: &["Disconnect", "Stop Session…"],
            },
            Case {
                name: "offline",
                hosts: vec![host("aa", SectionState::Disconnected), other],
                local: IN_PROCESS,
                policy: VerbPolicy::current(),
                expected: &["Connect"],
            },
            Case {
                name: "up to date",
                hosts: vec![
                    HostRow {
                        update: Some(&UP_TO_DATE),
                        ..connected
                    },
                    other,
                ],
                local: IN_PROCESS,
                policy: VerbPolicy::current(),
                expected: &["Restart Session…", "─", "Disconnect", "Stop Session…"],
            },
            Case {
                name: "available",
                hosts: vec![
                    HostRow {
                        update: Some(&AVAILABLE),
                        ..connected
                    },
                    other,
                ],
                local: IN_PROCESS,
                policy: VerbPolicy::current(),
                expected: &[
                    "Install Update…",
                    "Restart Session…",
                    "─",
                    "Disconnect",
                    "Stop Session…",
                ],
            },
            Case {
                name: "required over ssh",
                hosts: vec![
                    HostRow {
                        update: Some(&REQUIRED_SSH),
                        ..refused
                    },
                    other,
                ],
                local: IN_PROCESS,
                policy: VerbPolicy::current(),
                expected: &["Update roost-session…"],
            },
            Case {
                name: "required on localhost",
                hosts: vec![
                    HostRow {
                        transport: HostTransportKind::Localhost,
                        update: Some(&REQUIRED),
                        ..refused
                    },
                    other,
                ],
                local: IN_PROCESS,
                policy: VerbPolicy::current(),
                expected: &["Restart Session…"],
            },
            Case {
                name: "blocked",
                hosts: vec![
                    HostRow {
                        update: Some(&BLOCKED),
                        ..refused
                    },
                    other,
                ],
                local: IN_PROCESS,
                policy: VerbPolicy::current(),
                expected: &[],
            },
            Case {
                name: "fidelity update",
                hosts: vec![with_fidelity(FidelityAction::Update), other],
                local: IN_PROCESS,
                policy: VerbPolicy::current(),
                expected: &["Install Update…", "─", "Disconnect", "Stop Session…"],
            },
            Case {
                name: "fidelity restart",
                hosts: vec![with_fidelity(FidelityAction::Restart), other],
                local: IN_PROCESS,
                policy: VerbPolicy::current(),
                expected: &["Restart Session…", "─", "Disconnect", "Stop Session…"],
            },
            Case {
                name: "fidelity manual",
                hosts: vec![with_fidelity(FidelityAction::Manual), other],
                local: IN_PROCESS,
                policy: VerbPolicy::current(),
                expected: &["Disconnect", "Stop Session…"],
            },
            Case {
                name: "the local session slot",
                hosts: vec![slot, other],
                local: session,
                policy: VerbPolicy::current(),
                expected: &["Disconnect", "Stop Session…"],
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
                menu_rows_for(&verbs, "aa"),
                "{}",
                case.name
            );
            for entry in &menu {
                if let ContextEntry::Item { action, label, .. } = entry {
                    let verb = action.host_verb("aa").expect("a host action");
                    assert!(
                        verbs.iter().any(|row| row.surfaces.menu
                            && row.menu_label == label
                            && host_verbs::parse(&row.id).as_ref() == Some(&verb)),
                        "{}: {label} does not run its row's verb",
                        case.name
                    );
                }
            }
            assert_eq!(labels(&menu), case.expected, "{}", case.name);
            assert!(
                menu.iter()
                    .all(|entry| !matches!(entry, ContextEntry::Item { enabled: false, .. })),
                "{}: a host row is listed only when it applies",
                case.name
            );
        }
    }

    /// The order is [`HOST_ORDER`]'s whatever order `verbs` lists the
    /// rows in, with one separator before the ways to leave.
    #[test]
    fn host_items_follow_the_d6_table_order() {
        let row = |id: &str, label: &'static str| {
            let mut item = host_verbs::verbs(
                &[],
                &[],
                IN_PROCESS,
                VerbPolicy::current(),
                false,
                SlotHistory::Connected,
            )
            .remove(0);
            item.id = id.to_string();
            item.menu_label = label;
            item.surfaces.menu = true;
            item
        };
        let shuffled = [
            row("host:stop:aa", "Stop Session…"),
            row("host:restart:aa", "Restart Session…"),
            row("host:disconnect:aa", "Disconnect"),
            row("host:install:aa", "Install Update…"),
        ];
        let menu = entries(&ContextTarget::Host("aa".into()), &local_facts(), &shuffled);
        assert_eq!(
            actions(&menu),
            [
                Some(ContextAction::HostInstallUpdate),
                Some(ContextAction::HostRestartSession),
                None,
                Some(ContextAction::HostDisconnect),
                Some(ContextAction::HostStopSession),
            ]
        );
        assert_eq!(&HOST_ORDER[4..], &LEAVING);
    }

    /// A band menu opens with the session line, and a project's host
    /// block with the host's name and that line.
    #[test]
    fn the_session_line_heads_the_band_and_names_the_host_on_a_row() {
        let hosts = [host("aa", SectionState::Connected)];
        let verbs = host_verbs::verbs(
            &hosts,
            &[],
            IN_PROCESS,
            VerbPolicy::current(),
            false,
            SlotHistory::Connected,
        );
        let facts = ContextFacts {
            host: Some("aa"),
            interactive: true,
            on_this_machine: false,
            cwd_known: true,
            host_label: Some("mini3"),
            session_line: Some("Session 0.0.22 · up to date"),
        };
        let band = entries(&ContextTarget::Host("aa".into()), &facts, &verbs);
        assert_eq!(
            band[0],
            ContextEntry::Header("Session 0.0.22 · up to date".into())
        );
        let tab = entries(
            &ContextTarget::Tab(TabKey::new(HostId::new(2), 9)),
            &facts,
            &verbs,
        );
        assert_eq!(
            labels(&tab),
            ["Rename…", "New Tab Here", "Copy Path", "─", "Close Tab",],
            "a tab's menu lists no host block"
        );
        let dimmed = ContextFacts {
            interactive: false,
            ..facts
        };
        assert_eq!(
            labels(&entries(
                &ContextTarget::Tab(TabKey::new(HostId::new(2), 9)),
                &dimmed,
                &verbs
            )),
            [
                "mini3 · Session 0.0.22 · up to date",
                "Disconnect",
                "Stop Session…"
            ]
        );
    }
}
