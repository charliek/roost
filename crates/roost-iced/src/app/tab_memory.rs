//! Which tab a session-backed project lands on (plan 071 §D11).
//!
//! A session's own active tab is the tab last *opened* there — no
//! client's focus ever reaches the session — so on its own it says
//! nothing about what this window was looking at. What this window
//! showed is remembered per saved host in `state.json`
//! ([`HostTabMemory`]); the rules for reading and writing that memory are
//! these functions, over a mirror's rows.

use roost_engine::persistence::{HostTabMemory, TabMemo};
use roost_ipc::messages::Project;

/// A host's memory, read against the session behind the connection now.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Recall<'a> {
    memory: &'a HostTabMemory,
    same_session: bool,
}

/// `None` while the connection's session id is unknown: whether a
/// remembered tab id still names the same tab depends on it.
pub(crate) fn recall<'a>(
    memory: Option<&'a HostTabMemory>,
    session_id: Option<&str>,
) -> Option<Recall<'a>> {
    let session_id = session_id?;
    let memory = memory?;
    Some(Recall {
        memory,
        same_session: memory.session_id == session_id,
    })
}

impl Recall<'_> {
    /// The remembered tab as `project` lists it now: by id on the session
    /// that minted it, else by position ([`HostTabMemory`]).
    fn find(&self, project: &Project, memo: TabMemo) -> Option<i64> {
        let tab = if self.same_session {
            project.tabs.iter().find(|tab| tab.id == memo.tab_id)
        } else {
            usize::try_from(memo.position)
                .ok()
                .and_then(|at| project.tabs.get(at))
        };
        tab.map(|tab| tab.id)
    }

    fn last_viewed_in(&self, project: &Project) -> Option<i64> {
        self.find(project, *self.memory.last_viewed.get(&project.id)?)
    }

    /// The project the window was last showing and the tab to land on in
    /// it, while that project is still listed with tabs. A remembered tab
    /// that is gone yields the project's own preferred tab.
    pub(crate) fn last_shown(
        &self,
        projects: &[Project],
        active_tab_id: i64,
    ) -> Option<(i64, i64)> {
        let (project_id, memo) = self.memory.last_shown?;
        let project = projects.iter().find(|project| project.id == project_id)?;
        let tab = self
            .find(project, memo)
            .or_else(|| preferred_listed_tab(project, active_tab_id, None))?;
        Some((project.id, tab))
    }
}

/// The tab a host project row lands on — a click, ⌘1–9, the close
/// fallback: the one this window last showed there, else the session's
/// active tab when it is in that project, else the project's first.
pub(crate) fn preferred_listed_tab(
    project: &Project,
    active_tab_id: i64,
    recall: Option<Recall<'_>>,
) -> Option<i64> {
    recall
        .and_then(|recall| recall.last_viewed_in(project))
        .or_else(|| {
            project
                .tabs
                .iter()
                .find(|tab| tab.id == active_tab_id)
                .map(|tab| tab.id)
        })
        .or_else(|| project.tabs.first().map(|tab| tab.id))
}

/// The memory once the window shows `tab` in `project`, or `None` when
/// there is nothing to write: it is unchanged, the row is not listed, or
/// the session is not identified yet — writing then could keep a memory
/// stamped with a session that is gone, or reset one that is still valid.
///
/// A memory from another session is dropped first, and so are entries
/// for projects `projects` no longer lists.
pub(crate) fn remember_shown(
    stored: Option<&HostTabMemory>,
    session_id: Option<&str>,
    projects: &[Project],
    project: i64,
    tab: i64,
) -> Option<HostTabMemory> {
    let session_id = session_id?;
    let position = projects
        .iter()
        .find(|row| row.id == project)?
        .tabs
        .iter()
        .position(|row| row.id == tab)?;
    let memo = TabMemo {
        tab_id: tab,
        position: i32::try_from(position).ok()?,
    };
    let mut next = match stored {
        Some(stored) if stored.session_id == session_id => stored.clone(),
        _ => HostTabMemory {
            session_id: session_id.to_string(),
            ..HostTabMemory::default()
        },
    };
    next.last_viewed
        .retain(|id, _| projects.iter().any(|row| row.id == *id));
    next.last_viewed.insert(project, memo);
    next.last_shown = Some((project, memo));
    (stored != Some(&next)).then_some(next)
}

#[cfg(test)]
pub(super) mod fixtures {
    use roost_ipc::messages::{Project, Tab, TabState};

    pub(crate) fn project(id: i64, tabs: &[i64]) -> Project {
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
}

#[cfg(test)]
mod tests {
    use super::fixtures::project;
    use super::*;
    use std::collections::BTreeMap;

    fn memo(tab_id: i64, position: i32) -> TabMemo {
        TabMemo { tab_id, position }
    }

    /// `s-1` remembers tab 11 (position 1) in project 1 and tab 22
    /// (position 2) in project 2, and was last showing project 2.
    fn memory() -> HostTabMemory {
        HostTabMemory {
            session_id: "s-1".into(),
            last_viewed: BTreeMap::from([(1, memo(11, 1)), (2, memo(22, 2))]),
            last_shown: Some((2, memo(22, 2))),
        }
    }

    #[test]
    fn a_row_lands_on_the_tab_last_shown_there_on_the_same_session() {
        let row = project(1, &[10, 11, 12]);
        let memory = memory();
        let recall = recall(Some(&memory), Some("s-1"));

        assert_eq!(
            preferred_listed_tab(&row, 12, recall),
            Some(11),
            "the memory beats the session's own active tab and the first tab"
        );
        assert_eq!(
            preferred_listed_tab(&row, 12, None),
            Some(12),
            "without it, the session's active tab"
        );
        assert_eq!(
            preferred_listed_tab(&row, 99, None),
            Some(10),
            "else the first"
        );

        let closed = project(1, &[10, 12, 13]);
        assert_eq!(
            preferred_listed_tab(&closed, 13, recall),
            Some(13),
            "a remembered tab that closed is today's rule, not its old position"
        );
        let unvisited = project(3, &[30, 31]);
        assert_eq!(preferred_listed_tab(&unvisited, 0, recall), Some(30));
    }

    #[test]
    fn a_restarted_session_is_read_by_position_and_an_unknown_one_not_at_all() {
        // New ids: tab 11's id is not listed, and 13 happens to be.
        let row = project(1, &[13, 14, 15]);
        let memory = memory();

        assert_eq!(
            preferred_listed_tab(&row, 0, recall(Some(&memory), Some("s-2"))),
            Some(14),
            "position 1 on the session that replaced s-1"
        );
        assert_eq!(
            preferred_listed_tab(&project(1, &[13]), 0, recall(Some(&memory), Some("s-2"))),
            Some(13),
            "a position past the end is today's rule"
        );
        // Same ids as s-1 had, but the connection has not said which
        // session it reached.
        let same_ids = project(1, &[10, 11, 12]);
        assert_eq!(
            preferred_listed_tab(&same_ids, 0, recall(Some(&memory), None)),
            Some(10),
            "no session id, no memory"
        );
    }

    #[test]
    fn a_relaunch_lands_on_the_last_shown_project_by_id_or_by_position() {
        let rows = [project(1, &[10, 11]), project(2, &[20, 21, 22])];
        let memory = memory();

        let same = recall(Some(&memory), Some("s-1")).unwrap();
        assert_eq!(same.last_shown(&rows, 10), Some((2, 22)));

        let restarted = [project(1, &[40, 41]), project(2, &[50, 51, 52])];
        let other = recall(Some(&memory), Some("s-2")).unwrap();
        assert_eq!(
            other.last_shown(&restarted, 40),
            Some((2, 52)),
            "the project id survives a restart; the tab is found by position"
        );

        let closed = [project(1, &[10, 11]), project(2, &[20, 21])];
        assert_eq!(
            same.last_shown(&closed, 10),
            Some((2, 20)),
            "the project is kept when its remembered tab is gone"
        );
        assert_eq!(
            same.last_shown(&[project(1, &[10])], 10),
            None,
            "a deleted project is not somewhere to land"
        );
    }

    #[test]
    fn showing_a_tab_records_it_and_its_position() {
        let rows = [project(1, &[10, 11, 12])];
        let written = remember_shown(None, Some("s-1"), &rows, 1, 12).expect("a first write");
        assert_eq!(written.session_id, "s-1");
        assert_eq!(written.last_viewed, BTreeMap::from([(1, memo(12, 2))]));
        assert_eq!(written.last_shown, Some((1, memo(12, 2))));
    }

    #[test]
    fn nothing_is_written_when_nothing_changed() {
        let rows = [project(1, &[10, 11, 12]), project(2, &[20, 21, 22])];
        let stored = memory();
        assert_eq!(
            remember_shown(Some(&stored), Some("s-1"), &rows, 2, 22),
            None,
            "re-showing the last shown tab changes nothing"
        );
        assert!(remember_shown(Some(&stored), Some("s-1"), &rows, 1, 11).is_some());
    }

    #[test]
    fn nothing_is_written_for_an_unidentified_session_or_an_unlisted_row() {
        let rows = [project(1, &[10, 11])];
        let stored = memory();
        assert_eq!(remember_shown(Some(&stored), None, &rows, 1, 10), None);
        assert_eq!(remember_shown(None, Some("s-1"), &rows, 1, 99), None);
        assert_eq!(remember_shown(None, Some("s-1"), &rows, 9, 10), None);
    }

    #[test]
    fn a_write_prunes_deleted_projects_and_resets_a_replaced_session() {
        let stored = memory();

        let without_two = [project(1, &[10, 11])];
        let pruned = remember_shown(Some(&stored), Some("s-1"), &without_two, 1, 10).unwrap();
        assert_eq!(
            pruned.last_viewed,
            BTreeMap::from([(1, memo(10, 0))]),
            "project 2 is gone from the mirror, so from the memory"
        );

        let rows = [project(1, &[13, 14]), project(2, &[15])];
        let reset = remember_shown(Some(&stored), Some("s-2"), &rows, 1, 14).unwrap();
        assert_eq!(reset.session_id, "s-2");
        assert_eq!(
            reset.last_viewed,
            BTreeMap::from([(1, memo(14, 1))]),
            "nothing s-1 remembered survives into s-2's memory"
        );
    }
}
