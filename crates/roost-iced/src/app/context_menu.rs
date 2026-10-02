//! The right-click menu's one dispatcher (plan 073 D9).
//!
//! Every surface — the macOS popup, the Linux overlay, the
//! `app.context_menu_*` test ops — lists a row's menu with
//! [`App::context_entries`] and runs an item with
//! [`App::context_activate`]. Neither trusts what the menu showed when it
//! opened: an activation re-reads the row and rebuilds the menu first, so
//! an item whose row closed, whose host reconnected, or which stopped
//! applying is refused rather than run against whatever is there now.

use std::fmt;

use roost_ipc::messages::AppContextMenuEntry;
use roost_ui_model::context_menu::{
    self, ContextAction, ContextEntry, ContextFacts, ContextTarget,
};

use super::*;
use crate::url_launcher;

/// What `app.context_menu_open` answers where there is no overlay to
/// open.
pub(super) const OPEN_UNSUPPORTED: &str = if cfg!(target_os = "macos") {
    "app.context_menu_open is not supported on macOS: the menu is the native popup"
} else {
    "app.context_menu_open is not supported yet: the Linux overlay lands in plan 073 C10"
};

/// Why a menu item did not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ContextError {
    /// A modal, the palette, a rename editor or an IME composition owns
    /// input.
    Blocked,
    /// The row is gone, or names a connection that has since been
    /// replaced.
    Missing,
    /// A name off the wire that is no action at all.
    Unknown(String),
    Absent(ContextAction),
    Disabled(ContextAction),
    /// A local-backend switch is in flight (plan 063 §D8a).
    Busy,
    /// The action itself refused.
    Failed(String),
}

impl ContextError {
    pub(crate) fn failure(&self) -> HostOpFailure {
        let code = match self {
            Self::Busy => codes::BUSY,
            Self::Failed(_) => codes::INTERNAL,
            Self::Blocked
            | Self::Missing
            | Self::Unknown(_)
            | Self::Absent(_)
            | Self::Disabled(_) => codes::INVALID_PARAM,
        };
        HostOpFailure::new(code, self.to_string())
    }
}

impl fmt::Display for ContextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Blocked => {
                f.write_str("a dialog, the palette or an editor owns input; no menu item can run")
            }
            Self::Missing => f.write_str("the row this menu names no longer exists"),
            Self::Unknown(name) => write!(f, "{name:?} is not a context-menu action"),
            Self::Absent(action) => write!(f, "{} is not on this row's menu", action.as_str()),
            Self::Disabled(action) => {
                write!(f, "{} is disabled on this row's menu", action.as_str())
            }
            Self::Busy => f.write_str(roost_ipc::local_route::SWITCH_BUSY_MESSAGE),
            Self::Failed(message) => f.write_str(message),
        }
    }
}

/// What a target names right now, read off the live rows.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ContextRow {
    /// The row's project; `None` for a host band.
    project: Option<ProjectKey>,
    cwd: String,
    saved_host: Option<String>,
    on_this_machine: bool,
    interactive: bool,
}

impl ContextRow {
    fn facts(&self) -> ContextFacts<'_> {
        ContextFacts {
            host: self.saved_host.as_deref(),
            interactive: self.interactive,
            on_this_machine: self.on_this_machine,
            cwd_known: !self.cwd.is_empty(),
        }
    }
}

/// Resolve `target` against the in-process rows and every host band.
///
/// A key is matched on its whole `HostId`, so a key minted against a
/// connection that has since reconnected names nothing (`keys.rs`). A
/// dimmed host's rows still resolve, as not interactive.
fn context_row(
    target: &ContextTarget,
    local: &[Project],
    hosts: &[HostView],
) -> Option<ContextRow> {
    let instance = |host: HostId| {
        if host.is_local() {
            return Some((local, None));
        }
        let view = hosts.iter().find(|view| view.host == host)?;
        Some((view.projects.as_slice(), Some(view)))
    };
    let (project, cwd, view) = match target {
        ContextTarget::Host(saved_id) => {
            let view = hosts.iter().find(|view| &view.saved_id == saved_id)?;
            (None, "", Some(view))
        }
        ContextTarget::Tab(tab) => {
            let (rows, view) = instance(tab.host)?;
            let row = rows
                .iter()
                .find(|row| row.tabs.iter().any(|listed| listed.id == tab.tab))?;
            (
                Some(ProjectKey::new(tab.host, row.id)),
                listed_tab_cwd(row, tab.tab),
                view,
            )
        }
        ContextTarget::Project(project) => {
            let (rows, view) = instance(project.host)?;
            let row = rows.iter().find(|row| row.id == project.project)?;
            (Some(*project), row.cwd.as_str(), view)
        }
    };
    Some(ContextRow {
        project,
        cwd: cwd.to_string(),
        saved_host: view.map(|view| view.saved_id.clone()),
        on_this_machine: view.is_none_or(|view| view.transport.localhost()),
        interactive: view.is_none_or(|view| view.state.interactive()),
    })
}

/// The row `action` may run on: `menu` is that row and the menu it shows
/// now, `None` when the row is gone.
fn admit(
    blocked: bool,
    menu: Option<(ContextRow, Vec<ContextEntry>)>,
    action: ContextAction,
) -> Result<ContextRow, ContextError> {
    if blocked {
        return Err(ContextError::Blocked);
    }
    let (row, entries) = menu.ok_or(ContextError::Missing)?;
    match context_menu::item_enabled(&entries, action) {
        Some(true) => Ok(row),
        Some(false) => Err(ContextError::Disabled(action)),
        None => Err(ContextError::Absent(action)),
    }
}

pub(super) fn wire_entry(entry: &ContextEntry) -> AppContextMenuEntry {
    match entry {
        ContextEntry::Item {
            action,
            label,
            enabled,
        } => AppContextMenuEntry::Item {
            action: action.as_str().to_string(),
            label: label.clone(),
            enabled: *enabled,
        },
        ContextEntry::Separator => AppContextMenuEntry::Separator { separator: true },
    }
}

impl App {
    /// The menu `target` shows now; `None` when its row is gone.
    pub(crate) fn context_entries(&self, target: &ContextTarget) -> Option<Vec<ContextEntry>> {
        self.context_menu(target).map(|(_, entries)| entries)
    }

    fn context_menu(&self, target: &ContextTarget) -> Option<(ContextRow, Vec<ContextEntry>)> {
        let row = context_row(target, &self.projects, &self.host_views)?;
        // The palette's own inputs, so a host's items are the palette's
        // host rows and cannot disagree with them.
        let verbs = match row.saved_host {
            Some(_) => host_verbs::verbs(
                &self.host_verb_rows(),
                &self.host_recent_rows(),
                self.local_slot_input(),
                host_verbs::VerbPolicy::current(),
                self.switch_in_flight(),
                self.local_slot_history(),
            ),
            None => Vec::new(),
        };
        let entries = context_menu::entries(target, &row.facts(), &verbs);
        Some((row, entries))
    }

    /// Run one item of `target`'s menu, as it is now.
    pub(crate) fn context_activate(
        &mut self,
        target: &ContextTarget,
        action: ContextAction,
    ) -> Result<UiTask, ContextError> {
        let row = admit(self.context_blocked(), self.context_menu(target), action)?;
        if local_backend::context_action_mutates_local_backend(action) && self.switch_in_flight() {
            return Err(ContextError::Busy);
        }
        self.run_context_action(target, action, row)
    }

    fn context_blocked(&self) -> bool {
        self.text_capture() || self.palette.is_some()
    }

    fn run_context_action(
        &mut self,
        target: &ContextTarget,
        action: ContextAction,
        row: ContextRow,
    ) -> Result<UiTask, ContextError> {
        match (target, action) {
            (ContextTarget::Tab(tab), ContextAction::RenameTab) => {
                self.context_rename(RenameTarget::Tab(*tab))
            }
            (ContextTarget::Project(project), ContextAction::RenameProject) => {
                self.context_rename(RenameTarget::Project(*project))
            }
            (ContextTarget::Tab(tab), ContextAction::NewTabHere) => {
                let project = row.project.ok_or(ContextError::Missing)?;
                Ok(self.new_tab_in(project, Some(*tab)))
            }
            (ContextTarget::Project(project), ContextAction::NewTab) => {
                Ok(self.new_tab_in(*project, None))
            }
            (ContextTarget::Tab(tab), ContextAction::CloseTab) => Ok(self.close_tab(*tab)),
            (ContextTarget::Project(project), ContextAction::CloseProject) => {
                self.confirm_close_project(*project)
                    .map_err(ContextError::Failed)?;
                Ok(UiTask::None)
            }
            (ContextTarget::Tab(_), ContextAction::CopyTabPath)
            | (ContextTarget::Project(_), ContextAction::CopyProjectPath) => {
                self.clipboard.enqueue_write(ClipboardOp::System, row.cwd);
                Ok(self.clipboard.start_next())
            }
            (ContextTarget::Project(_), ContextAction::OpenProjectFolder) => {
                let url = url_launcher::file_url(Path::new(&row.cwd));
                Ok(self.open_external(url_launcher::External::Url(url)))
            }
            _ => {
                let verb = row
                    .saved_host
                    .and_then(|saved_id| action.host_verb(&saved_id))
                    .ok_or(ContextError::Absent(action))?;
                self.run_host_verb(verb, Self::CLICK_ACTIVATION_ORIGIN)
                    .map(|dispatch| dispatch.task)
                    .map_err(ContextError::Failed)
            }
        }
    }

    fn context_rename(&mut self, target: RenameTarget) -> Result<UiTask, ContextError> {
        self.begin_rename_target(target)
            .map_err(ContextError::Failed)?;
        Ok(self.take_rename_focus_task())
    }
}

#[cfg(test)]
mod tests {
    use roost_ipc::messages::{Tab, TabState};

    use super::*;

    fn tab(id: i64, project_id: i64, cwd: &str) -> Tab {
        Tab {
            id,
            project_id,
            title: String::new(),
            cwd: cwd.into(),
            state: TabState::None,
            has_notification: false,
            is_active: false,
            user_titled: false,
            position: 0,
            created_at: 0,
            last_active: 0,
            hook_active: false,
            shell_state: Default::default(),
            agent_lifecycle: Default::default(),
            ownership: None,
        }
    }

    fn project(id: i64, cwd: &str, tabs: Vec<Tab>) -> Project {
        Project {
            id,
            name: format!("p{id}"),
            cwd: cwd.into(),
            position: 0,
            created_at: 0,
            tabs,
        }
    }

    fn view(saved_id: &str, host: HostId, state: host_sidebar::SectionState) -> HostView {
        HostView {
            saved_id: saved_id.into(),
            label: saved_id.into(),
            target: saved_id.into(),
            transport: host_sidebar::HostTransportKind::Ssh,
            host,
            state,
            reduced_fidelity: false,
            reason: None,
            projects: vec![project(3, "/srv/p3", vec![tab(7, 3, "/srv/p3/t7")])],
            active_tab_id: 7,
            agents: 0,
        }
    }

    fn activation(
        blocked: bool,
        target: &ContextTarget,
        local: &[Project],
        hosts: &[HostView],
        action: ContextAction,
    ) -> Result<(), ContextError> {
        let menu = context_row(target, local, hosts).map(|row| {
            let entries = context_menu::entries(target, &row.facts(), &[]);
            (row, entries)
        });
        admit(blocked, menu, action).map(drop)
    }

    #[test]
    fn an_item_on_a_row_of_a_replaced_connection_is_refused() {
        let hosts = [view(
            "aa",
            HostId::new(5),
            host_sidebar::SectionState::Connected,
        )];
        let stale = HostId::new(4);
        for target in [
            ContextTarget::Tab(TabKey::new(stale, 7)),
            ContextTarget::Project(ProjectKey::new(stale, 3)),
        ] {
            assert_eq!(
                activation(false, &target, &[], &hosts, ContextAction::CloseTab),
                Err(ContextError::Missing),
                "{target:?}"
            );
        }
        let live = ContextTarget::Tab(TabKey::new(HostId::new(5), 7));
        assert_eq!(
            activation(false, &live, &[], &hosts, ContextAction::CloseTab),
            Ok(())
        );
        assert_eq!(
            context_row(&live, &[], &hosts).map(|row| (row.project, row.cwd, row.saved_host)),
            Some((
                Some(ProjectKey::new(HostId::new(5), 3)),
                "/srv/p3/t7".to_string(),
                Some("aa".to_string())
            ))
        );
    }

    #[test]
    fn an_item_on_a_closed_tab_is_refused() {
        let local = [project(1, "/tmp", vec![tab(2, 1, "")])];
        assert_eq!(
            activation(
                false,
                &ContextTarget::Tab(TabKey::local(9)),
                &local,
                &[],
                ContextAction::CloseTab
            ),
            Err(ContextError::Missing)
        );
        let open = ContextTarget::Tab(TabKey::local(2));
        assert_eq!(
            activation(false, &open, &local, &[], ContextAction::CloseTab),
            Ok(())
        );
        assert_eq!(
            context_row(&open, &local, &[]).map(|row| row.cwd),
            Some("/tmp".to_string()),
            "a tab with no cwd of its own copies its project's"
        );
    }

    #[test]
    fn an_item_that_no_longer_applies_is_refused() {
        let local = [project(1, "", vec![])];
        let target = ContextTarget::Project(ProjectKey::local(1));
        assert_eq!(
            activation(false, &target, &local, &[], ContextAction::CopyProjectPath),
            Err(ContextError::Disabled(ContextAction::CopyProjectPath))
        );
        assert_eq!(
            activation(false, &target, &local, &[], ContextAction::CloseTab),
            Err(ContextError::Absent(ContextAction::CloseTab))
        );
        let dimmed = [view(
            "aa",
            HostId::new(5),
            host_sidebar::SectionState::Disconnected,
        )];
        let row = ContextTarget::Project(ProjectKey::new(HostId::new(5), 3));
        assert_eq!(
            activation(false, &row, &[], &dimmed, ContextAction::RenameProject),
            Err(ContextError::Absent(ContextAction::RenameProject)),
            "a dimmed host's row offers only its host's verbs"
        );
    }

    #[test]
    fn no_item_runs_while_something_else_owns_input() {
        let local = [project(1, "/tmp", vec![tab(2, 1, "/tmp")])];
        let target = ContextTarget::Tab(TabKey::local(2));
        assert_eq!(
            activation(true, &target, &local, &[], ContextAction::RenameTab),
            Err(ContextError::Blocked)
        );
        assert_eq!(
            activation(false, &target, &local, &[], ContextAction::RenameTab),
            Ok(())
        );
    }

    #[test]
    fn refusals_are_invalid_param_with_distinct_messages() {
        let refusals = [
            ContextError::Blocked,
            ContextError::Missing,
            ContextError::Unknown("remove_host".into()),
            ContextError::Absent(ContextAction::CloseTab),
            ContextError::Disabled(ContextAction::CloseTab),
        ];
        let messages: HashSet<String> = refusals
            .iter()
            .map(|refusal| {
                let failure = refusal.failure();
                assert_eq!(failure.code, codes::INVALID_PARAM, "{refusal:?}");
                failure.message
            })
            .collect();
        assert_eq!(messages.len(), refusals.len());
        let busy = ContextError::Busy.failure();
        assert_eq!(
            (busy.code.as_str(), busy.message.as_str()),
            (codes::BUSY, roost_ipc::local_route::SWITCH_BUSY_MESSAGE)
        );
    }

    #[test]
    fn a_host_band_resolves_by_saved_id_even_before_it_ever_connected() {
        let never = view(
            "bb",
            HostId::LOCAL,
            host_sidebar::SectionState::Disconnected,
        );
        let row = context_row(&ContextTarget::Host("bb".into()), &[], &[never]).expect("listed");
        assert_eq!(row.saved_host.as_deref(), Some("bb"));
        assert_eq!(row.project, None);
        assert_eq!(
            context_row(&ContextTarget::Host("cc".into()), &[], &[]),
            None
        );
    }

    #[test]
    fn entries_go_on_the_wire_as_items_and_separators() {
        assert_eq!(
            wire_entry(&ContextEntry::Item {
                action: ContextAction::NewTabHere,
                label: "New Tab Here".into(),
                enabled: false,
            }),
            AppContextMenuEntry::Item {
                action: "new_tab_here".into(),
                label: "New Tab Here".into(),
                enabled: false,
            }
        );
        assert_eq!(
            wire_entry(&ContextEntry::Separator),
            AppContextMenuEntry::Separator { separator: true }
        );
    }
}
