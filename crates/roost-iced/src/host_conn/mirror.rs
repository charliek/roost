//! One host's workspace, mirrored client-side.
//!
//! The session is the authority; this is a projection of it, built from
//! a `tab.list` fenced against the event stream and moved forward only
//! by the batches that follow. Nothing here is optimistic — a row
//! appears when its `tab.opened` lands, which is exactly the
//! event-confirmed rule plan 037 §3.9 pins.
//!
//! Pure data, so it is unit-testable without a socket. It models the
//! workspace facts only; the envelopes it does not model (`tab.effect`,
//! `notification.fired`) travel beside it on the feed for C5/C8 to
//! route.
//!
//! [`SharedMirror`] is how the connection task and the UI share one:
//! the task writes it in place and the feed carries only a
//! notification, so a high-churn host cannot pile full-workspace clones
//! onto an unbounded channel.

use std::sync::{Mutex, MutexGuard};

use roost_ipc::messages::{
    ops, ActiveChangedEvent, AgentReportChangedEvent, EventBatch, EventEnvelope,
    HookActiveChangedEvent, Project, ProjectCreatedEvent, ProjectDeletedEvent, ProjectRenamedEvent,
    ProjectsReorderedEvent, Tab, TabClosedEvent, TabCwdChangedEvent, TabListResult,
    TabNotificationEvent, TabOpenedEvent, TabStateChangedEvent, TabTitleChangedEvent,
    TabsReorderedEvent,
};

/// A host's projects and tabs, plus which of them the session considers
/// active.
///
/// Projects are held in the session's own display order and tabs in
/// theirs, so a consumer renders by iterating rather than by sorting —
/// the reorder events carry the full post-reorder order, which is what
/// makes that possible.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct HostMirror {
    pub(crate) projects: Vec<Project>,
    pub(crate) active_project_id: i64,
    pub(crate) active_tab_id: i64,
    /// The last commit folded in. The fence at build time, then every
    /// applied batch's revision.
    pub(crate) revision: u64,
}

impl HostMirror {
    /// Build from a `tab.list`, fenced at `revision`.
    ///
    /// The fence is the caller's: `ipc.md` #eventssubscribe pairs the
    /// subscribe ack with the snapshot's own `revision`, and the higher
    /// of the two is the only safe reading when a session omits the
    /// snapshot's.
    pub(crate) fn from_list(list: TabListResult, revision: u64) -> Self {
        let active = active_from(&list.projects);
        Self {
            projects: list.projects,
            active_project_id: active.0,
            active_tab_id: active.1,
            revision,
        }
    }

    /// Apply one batch. Returns `false` when the batch is at or below
    /// the fence and was discarded.
    ///
    /// Discarding rather than skipping the whole stream matters: the
    /// snapshot is taken *after* the subscription, so the first few
    /// batches routinely describe commits the snapshot already contains.
    pub(crate) fn apply_batch(&mut self, batch: &EventBatch) -> bool {
        if batch.revision <= self.revision {
            return false;
        }
        for envelope in &batch.events {
            self.apply_event(envelope);
        }
        self.revision = batch.revision;
        true
    }

    /// Fold one envelope in. Unknown names — and the envelopes that
    /// carry no workspace fact — are ignored here rather than rejected:
    /// a newer session's extra event must not break a mirror, and
    /// `tab.effect` is C5's to apply, not the mirror's to model.
    pub(crate) fn apply_event(&mut self, envelope: &EventEnvelope) {
        // Every arm decodes its own payload; a payload that does not
        // decode is a schema drift worth a log line, never a panic and
        // never a partially applied event.
        macro_rules! decode {
            ($ty:ty) => {
                match serde_json::from_value::<$ty>(envelope.data.clone()) {
                    Ok(data) => data,
                    Err(error) => {
                        tracing::debug!(
                            event = %envelope.event,
                            %error,
                            "host mirror could not decode an event payload"
                        );
                        return;
                    }
                }
            };
        }

        match envelope.event.as_str() {
            ops::EVENT_TAB_OPENED => {
                let data = decode!(TabOpenedEvent);
                self.insert_tab(data.tab);
            }
            ops::EVENT_TAB_CLOSED => {
                let data = decode!(TabClosedEvent);
                for project in &mut self.projects {
                    project.tabs.retain(|tab| tab.id != data.tab_id);
                }
                if self.active_tab_id == data.tab_id {
                    self.active_tab_id = 0;
                }
            }
            ops::EVENT_TAB_STATE_CHANGED => {
                let data = decode!(TabStateChangedEvent);
                self.with_tab(data.tab_id, |tab| tab.state = data.state);
            }
            ops::EVENT_TAB_TITLE_CHANGED => {
                let data = decode!(TabTitleChangedEvent);
                self.with_tab(data.tab_id, |tab| tab.title = data.title.clone());
            }
            ops::EVENT_TAB_CWD_CHANGED => {
                let data = decode!(TabCwdChangedEvent);
                self.with_tab(data.tab_id, |tab| tab.cwd = data.cwd.clone());
            }
            ops::EVENT_TAB_NOTIFICATION => {
                let data = decode!(TabNotificationEvent);
                self.with_tab(data.tab_id, |tab| tab.has_notification = data.has_pending);
            }
            ops::EVENT_HOOK_ACTIVE_CHANGED => {
                let data = decode!(HookActiveChangedEvent);
                self.with_tab(data.tab_id, |tab| tab.hook_active = data.active);
            }
            ops::EVENT_AGENT_REPORT_CHANGED => {
                let data = decode!(AgentReportChangedEvent);
                self.with_tab(data.tab_id, |tab| {
                    tab.shell_state = data.shell_state;
                    tab.agent_lifecycle = data.agent_lifecycle;
                    tab.ownership = data.ownership.clone();
                    // `state` and `hook_active` ride pre-derived so a
                    // subscriber never re-runs the projection.
                    tab.state = data.state;
                    tab.hook_active = data.hook_active;
                });
            }
            ops::EVENT_PROJECT_CREATED => {
                let data = decode!(ProjectCreatedEvent);
                if let Some(existing) = self
                    .projects
                    .iter_mut()
                    .find(|project| project.id == data.project.id)
                {
                    // Keep the tabs: `project.created` ships an empty
                    // list by contract, and a duplicate must not empty a
                    // project that already has rows.
                    let tabs = std::mem::take(&mut existing.tabs);
                    *existing = data.project;
                    existing.tabs = tabs;
                } else {
                    self.projects.push(data.project);
                }
            }
            ops::EVENT_PROJECT_RENAMED => {
                let data = decode!(ProjectRenamedEvent);
                if let Some(project) = self
                    .projects
                    .iter_mut()
                    .find(|project| project.id == data.project_id)
                {
                    project.name = data.name;
                }
            }
            ops::EVENT_PROJECT_DELETED => {
                let data = decode!(ProjectDeletedEvent);
                self.projects
                    .retain(|project| project.id != data.project_id);
                if self.active_project_id == data.project_id {
                    self.active_project_id = 0;
                    self.active_tab_id = 0;
                }
            }
            ops::EVENT_ACTIVE_CHANGED => {
                let data = decode!(ActiveChangedEvent);
                self.active_project_id = data.project_id;
                self.active_tab_id = data.tab_id;
            }
            ops::EVENT_TABS_REORDERED => {
                let data = decode!(TabsReorderedEvent);
                if let Some(project) = self
                    .projects
                    .iter_mut()
                    .find(|project| project.id == data.project_id)
                {
                    reorder(&mut project.tabs, &data.tab_ids, |tab| tab.id);
                }
            }
            ops::EVENT_PROJECTS_REORDERED => {
                let data = decode!(ProjectsReorderedEvent);
                reorder(&mut self.projects, &data.project_ids, |project| project.id);
            }
            // `notification.fired` and `tab.effect` carry no workspace
            // fact; they ride the feed beside the mirror.
            _ => {}
        }
    }

    /// Every tab across every project, in display order.
    pub(crate) fn tabs(&self) -> impl Iterator<Item = &Tab> {
        self.projects.iter().flat_map(|project| project.tabs.iter())
    }

    pub(crate) fn tab(&self, tab_id: i64) -> Option<&Tab> {
        self.tabs().find(|tab| tab.id == tab_id)
    }

    fn with_tab(&mut self, tab_id: i64, apply: impl FnOnce(&mut Tab)) {
        if let Some(tab) = self
            .projects
            .iter_mut()
            .flat_map(|project| project.tabs.iter_mut())
            .find(|tab| tab.id == tab_id)
        {
            apply(tab);
        }
    }

    /// Place a tab under its project, replacing any row with the same
    /// id. A `tab.opened` for a project this mirror has never seen is
    /// dropped rather than inventing a project — the `project.created`
    /// that names it rides the same batch, and a mirror that had neither
    /// is one that needs a resync, not a guess.
    fn insert_tab(&mut self, tab: Tab) {
        for project in &mut self.projects {
            project.tabs.retain(|existing| existing.id != tab.id);
        }
        let Some(project) = self
            .projects
            .iter_mut()
            .find(|project| project.id == tab.project_id)
        else {
            tracing::debug!(
                tab = tab.id,
                project = tab.project_id,
                "host mirror saw a tab for an unknown project"
            );
            return;
        };
        // The event carries the tab's `position`, so insert by it rather
        // than appending: a reopened tab lands where it belongs without
        // waiting for a `tabs.reordered`. A tie keeps the row that was
        // already there in front; the server's own reorder event is the
        // authority whenever that guess is wrong.
        let at = project
            .tabs
            .iter()
            .position(|existing| existing.position > tab.position)
            .unwrap_or(project.tabs.len());
        project.tabs.insert(at, tab);
    }
}

/// One incarnation's mirror, as the connection task and the UI share it.
///
/// The task is the only writer and applies every batch in place; the UI
/// reads whatever is current when it drains the feed. Nothing is cloned
/// onto the feed — a host that commits thousands of times a second would
/// otherwise put one full-workspace clone per commit on an *unbounded*
/// channel, and the UI only ever wants the latest state anyway. The feed
/// carries the change notification (and the verbatim envelopes C5
/// routes); the state lives here.
///
/// A consequence worth stating: by the time a drain reads this, it may
/// already be past the revision the item that woke it named. That is the
/// point — the wakes coalesce, and the mirror is the authority. Anything
/// that must be exact per-commit (C5's `tab.effect`) rides the feed item
/// itself, not this.
#[derive(Debug, Default)]
pub(crate) struct SharedMirror {
    state: Mutex<HostMirror>,
}

impl SharedMirror {
    pub(crate) fn new(mirror: HostMirror) -> Self {
        Self {
            state: Mutex::new(mirror),
        }
    }

    /// The current state. Held for a render walk or an accessor and
    /// never across an await — the writer only ever holds it for one
    /// batch, so contention is a few microseconds at worst.
    pub(crate) fn read(&self) -> MutexGuard<'_, HostMirror> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Replace the whole mirror: a fresh snapshot after a connect or a
    /// resync, which is purge-then-rebuild by contract.
    pub(crate) fn reset(&self, mirror: HostMirror) {
        *self.read() = mirror;
    }

    /// Fold one batch in. `false` means it was at or below the fence and
    /// nothing changed.
    pub(crate) fn apply_batch(&self, batch: &EventBatch) -> bool {
        self.read().apply_batch(batch)
    }
}

/// The session's active pair, read off a snapshot's `is_active` flags.
fn active_from(projects: &[Project]) -> (i64, i64) {
    for project in projects {
        if let Some(tab) = project.tabs.iter().find(|tab| tab.is_active) {
            return (project.id, tab.id);
        }
    }
    (0, 0)
}

/// Reorder `items` to match `order`, keeping anything `order` does not
/// name at the end in its current relative position.
///
/// The events carry the *full* post-reorder order, so the tail should
/// normally be empty; keeping it anyway means a mirror that raced a
/// concurrent open never silently drops the row it had.
fn reorder<T>(items: &mut [T], order: &[i64], id: impl Fn(&T) -> i64) {
    let rank: std::collections::HashMap<i64, usize> = order
        .iter()
        .enumerate()
        .map(|(at, wanted)| (*wanted, at))
        .collect();
    // A stable sort is what keeps the unnamed tail in its current
    // relative order; `usize::MAX` is what puts it at the end.
    items.sort_by_key(|item| rank.get(&id(item)).copied().unwrap_or(usize::MAX));
}

#[cfg(test)]
mod tests {
    use super::*;
    use roost_ipc::agent::{AgentLifecycle, ShellState};
    use roost_ipc::messages::TabState;

    fn tab(id: i64, project_id: i64, position: i32) -> Tab {
        Tab {
            id,
            project_id,
            title: format!("tab-{id}"),
            cwd: "/tmp".into(),
            state: TabState::None,
            has_notification: false,
            is_active: false,
            user_titled: false,
            position,
            created_at: 0,
            last_active: 0,
            hook_active: false,
            shell_state: ShellState::default(),
            agent_lifecycle: AgentLifecycle::default(),
            ownership: None,
        }
    }

    fn project(id: i64, tabs: Vec<Tab>) -> Project {
        Project {
            id,
            name: format!("project-{id}"),
            cwd: "/tmp".into(),
            position: id as i32,
            created_at: 0,
            tabs,
        }
    }

    fn mirror() -> HostMirror {
        HostMirror::from_list(
            TabListResult {
                projects: vec![project(1, vec![tab(10, 1, 0), tab(11, 1, 1)])],
                revision: Some(42),
            },
            42,
        )
    }

    fn event(name: &str, data: serde_json::Value) -> EventEnvelope {
        EventEnvelope {
            event: name.into(),
            data,
        }
    }

    fn batch(revision: u64, events: Vec<EventEnvelope>) -> EventBatch {
        EventBatch { revision, events }
    }

    #[test]
    fn a_snapshot_carries_its_fence_and_the_active_pair() {
        let mut list = TabListResult {
            projects: vec![project(1, vec![tab(10, 1, 0)])],
            revision: Some(7),
        };
        list.projects[0].tabs[0].is_active = true;
        let mirror = HostMirror::from_list(list, 7);
        assert_eq!(mirror.revision, 7);
        assert_eq!((mirror.active_project_id, mirror.active_tab_id), (1, 10));
    }

    /// The fence rule, which is the whole point of taking the snapshot
    /// after the subscribe: batches the snapshot already contains are
    /// discarded, and the first one past it is applied.
    #[test]
    fn batches_at_or_below_the_fence_are_discarded() {
        let mut mirror = mirror();
        let opened = event(
            ops::EVENT_TAB_OPENED,
            serde_json::json!({"tab": tab(12, 1, 2)}),
        );

        assert!(!mirror.apply_batch(&batch(41, vec![opened.clone()])));
        assert!(!mirror.apply_batch(&batch(42, vec![opened.clone()])));
        assert_eq!(mirror.tabs().count(), 2, "nothing below the fence applied");

        assert!(mirror.apply_batch(&batch(43, vec![opened])));
        assert_eq!(mirror.tabs().count(), 3);
        assert_eq!(mirror.revision, 43);
    }

    /// What a revision gap costs and what recovers it: the client cannot
    /// reconstruct what it missed, so the resync throws the mirror away
    /// and rebuilds from a fresh `tab.list` at the new fence. A merge
    /// would keep rows the session has since closed.
    #[test]
    fn a_resync_rebuilds_the_mirror_rather_than_merging_into_it() {
        let mut mirror = mirror();
        mirror.apply_batch(&batch(
            43,
            vec![event(
                ops::EVENT_TAB_TITLE_CHANGED,
                serde_json::json!({"tab_id": "10", "title": "stale"}),
            )],
        ));
        assert_eq!(mirror.tabs().count(), 2);

        // The gap: revision 44 never arrived, 45 did. The recovery is a
        // fresh snapshot at a fresh fence — in which tab 11 is gone and
        // a project the mirror never saw exists.
        mirror = HostMirror::from_list(
            TabListResult {
                projects: vec![
                    project(1, vec![tab(10, 1, 0)]),
                    project(2, vec![tab(20, 2, 0)]),
                ],
                revision: Some(45),
            },
            45,
        );

        assert_eq!(mirror.revision, 45);
        assert!(mirror.tab(11).is_none(), "a closed tab does not survive");
        assert!(mirror.tab(20).is_some(), "and a new one arrives");
        assert_eq!(
            mirror.tab(10).unwrap().title,
            "tab-10",
            "the server's copy wins over anything the old mirror held"
        );
        assert!(
            !mirror.apply_batch(&batch(45, vec![])),
            "the new fence holds against the batches it already covers"
        );
        assert!(mirror.apply_batch(&batch(46, vec![])));
    }

    #[test]
    fn an_empty_batch_still_advances_the_revision() {
        let mut mirror = mirror();
        assert!(mirror.apply_batch(&batch(43, vec![])));
        assert_eq!(mirror.revision, 43);
    }

    #[test]
    fn a_tab_opens_at_its_position_and_closes_out_of_every_project() {
        let mut mirror = mirror();
        mirror.apply_event(&event(
            ops::EVENT_TAB_OPENED,
            serde_json::json!({"tab": tab(9, 1, 0)}),
        ));
        assert_eq!(
            mirror.projects[0]
                .tabs
                .iter()
                .map(|t| t.id)
                .collect::<Vec<_>>(),
            vec![10, 9, 11],
            "it lands before the first row that outranks it; a tie keeps \
             the row that was already there in front"
        );

        mirror.apply_event(&event(
            ops::EVENT_TAB_CLOSED,
            serde_json::json!({"tab_id": "10"}),
        ));
        assert!(mirror.tab(10).is_none());
        assert_eq!(mirror.tabs().count(), 2);
    }

    #[test]
    fn reopening_a_tab_id_replaces_rather_than_duplicates() {
        let mut mirror = mirror();
        let mut moved = tab(10, 1, 5);
        moved.title = "renamed".into();
        mirror.apply_event(&event(
            ops::EVENT_TAB_OPENED,
            serde_json::json!({ "tab": moved }),
        ));
        assert_eq!(mirror.tabs().count(), 2);
        assert_eq!(mirror.tab(10).unwrap().title, "renamed");
    }

    #[test]
    fn a_tab_for_an_unknown_project_is_dropped_not_invented() {
        let mut mirror = mirror();
        mirror.apply_event(&event(
            ops::EVENT_TAB_OPENED,
            serde_json::json!({"tab": tab(30, 99, 0)}),
        ));
        assert_eq!(mirror.projects.len(), 1);
        assert!(mirror.tab(30).is_none());
    }

    #[test]
    fn titles_cwds_states_and_notifications_land_on_their_tab() {
        let mut mirror = mirror();
        mirror.apply_event(&event(
            ops::EVENT_TAB_TITLE_CHANGED,
            serde_json::json!({"tab_id": "11", "title": "vim"}),
        ));
        mirror.apply_event(&event(
            ops::EVENT_TAB_CWD_CHANGED,
            serde_json::json!({"tab_id": "11", "cwd": "/src"}),
        ));
        mirror.apply_event(&event(
            ops::EVENT_TAB_STATE_CHANGED,
            serde_json::json!({"tab_id": "11", "state": "running"}),
        ));
        mirror.apply_event(&event(
            ops::EVENT_TAB_NOTIFICATION,
            serde_json::json!({"tab_id": "11", "has_pending": true}),
        ));
        mirror.apply_event(&event(
            ops::EVENT_HOOK_ACTIVE_CHANGED,
            serde_json::json!({"tab_id": "11", "active": true}),
        ));

        let tab = mirror.tab(11).unwrap();
        assert_eq!(tab.title, "vim");
        assert_eq!(tab.cwd, "/src");
        assert_eq!(tab.state, TabState::Running);
        assert!(tab.has_notification);
        assert!(tab.hook_active);
        // The untouched tab is untouched.
        assert_eq!(mirror.tab(10).unwrap().title, "tab-10");
    }

    #[test]
    fn an_agent_report_carries_all_three_axes_and_the_derived_pair() {
        let mut mirror = mirror();
        mirror.apply_event(&event(
            ops::EVENT_AGENT_REPORT_CHANGED,
            serde_json::json!({
                "tab_id": "10",
                "shell_state": "foreground_process",
                "agent_lifecycle": "working",
                "state": "running",
                "hook_active": true,
            }),
        ));
        let tab = mirror.tab(10).unwrap();
        assert_eq!(tab.shell_state, ShellState::ForegroundProcess);
        assert_eq!(tab.agent_lifecycle, AgentLifecycle::Working);
        assert_eq!(tab.state, TabState::Running);
        assert!(tab.hook_active);
    }

    #[test]
    fn projects_are_created_renamed_and_deleted() {
        let mut mirror = mirror();
        mirror.apply_event(&event(
            ops::EVENT_PROJECT_CREATED,
            serde_json::json!({"project": project(2, vec![])}),
        ));
        assert_eq!(mirror.projects.len(), 2);

        mirror.apply_event(&event(
            ops::EVENT_PROJECT_RENAMED,
            serde_json::json!({"project_id": "2", "name": "renamed"}),
        ));
        assert_eq!(mirror.projects[1].name, "renamed");

        mirror.apply_event(&event(
            ops::EVENT_PROJECT_DELETED,
            serde_json::json!({"project_id": "1"}),
        ));
        assert_eq!(mirror.projects.len(), 1);
        assert_eq!(mirror.projects[0].id, 2);
    }

    /// `project.created` ships an empty tab list by contract, so a
    /// duplicate must not empty a project that already has rows.
    #[test]
    fn a_duplicate_project_created_keeps_the_tabs_it_already_had() {
        let mut mirror = mirror();
        mirror.apply_event(&event(
            ops::EVENT_PROJECT_CREATED,
            serde_json::json!({"project": project(1, vec![])}),
        ));
        assert_eq!(mirror.projects.len(), 1);
        assert_eq!(mirror.projects[0].tabs.len(), 2);
    }

    #[test]
    fn reorder_events_carry_the_whole_order() {
        let mut mirror = HostMirror::from_list(
            TabListResult {
                projects: vec![
                    project(1, vec![tab(10, 1, 0), tab(11, 1, 1), tab(12, 1, 2)]),
                    project(2, vec![]),
                ],
                revision: Some(1),
            },
            1,
        );

        mirror.apply_event(&event(
            ops::EVENT_TABS_REORDERED,
            serde_json::json!({"project_id": "1", "tab_ids": ["12", "10", "11"]}),
        ));
        assert_eq!(
            mirror.projects[0]
                .tabs
                .iter()
                .map(|t| t.id)
                .collect::<Vec<_>>(),
            vec![12, 10, 11]
        );

        mirror.apply_event(&event(
            ops::EVENT_PROJECTS_REORDERED,
            serde_json::json!({"project_ids": ["2", "1"]}),
        ));
        assert_eq!(
            mirror.projects.iter().map(|p| p.id).collect::<Vec<_>>(),
            vec![2, 1]
        );
    }

    /// A row the order does not name survives at the tail rather than
    /// vanishing — a reorder that raced an open must not lose the tab.
    #[test]
    fn a_reorder_that_omits_a_row_keeps_it() {
        let mut mirror = mirror();
        mirror.apply_event(&event(
            ops::EVENT_TABS_REORDERED,
            serde_json::json!({"project_id": "1", "tab_ids": ["11"]}),
        ));
        assert_eq!(
            mirror.projects[0]
                .tabs
                .iter()
                .map(|t| t.id)
                .collect::<Vec<_>>(),
            vec![11, 10]
        );
    }

    #[test]
    fn active_changed_moves_the_pair_and_a_close_clears_it() {
        let mut mirror = mirror();
        mirror.apply_event(&event(
            ops::EVENT_ACTIVE_CHANGED,
            serde_json::json!({"project_id": "1", "tab_id": "11"}),
        ));
        assert_eq!((mirror.active_project_id, mirror.active_tab_id), (1, 11));

        mirror.apply_event(&event(
            ops::EVENT_TAB_CLOSED,
            serde_json::json!({"tab_id": "11"}),
        ));
        assert_eq!(mirror.active_tab_id, 0);
        assert_eq!(mirror.active_project_id, 1);
    }

    /// Additive server-side events must not break a mirror that predates
    /// them, and `tab.effect` is deliberately not modeled here.
    #[test]
    fn unmodeled_and_unknown_events_leave_the_mirror_alone() {
        let mut mirror = mirror();
        let before = mirror.clone();
        for envelope in [
            event(
                ops::EVENT_TAB_EFFECT,
                serde_json::json!({"tab_id": "10", "effect": "bell"}),
            ),
            event(
                ops::EVENT_NOTIFICATION_FIRED,
                serde_json::json!({"tab_id": "10", "title": "done", "body": ""}),
            ),
            event("some.future.event", serde_json::json!({"anything": true})),
        ] {
            mirror.apply_event(&envelope);
        }
        assert_eq!(mirror, before);
    }

    /// The shared handle is the same mirror with a lock around it: a
    /// write lands in place and a reader that came in later sees it,
    /// with no clone crossing the feed.
    #[test]
    fn a_shared_mirror_is_written_in_place_and_read_by_whoever_holds_it() {
        let shared = std::sync::Arc::new(SharedMirror::new(mirror()));
        let held = std::sync::Arc::clone(&shared);

        assert!(shared.apply_batch(&batch(
            43,
            vec![event(
                ops::EVENT_TAB_TITLE_CHANGED,
                serde_json::json!({"tab_id": "10", "title": "vim"}),
            )],
        )));
        assert_eq!(held.read().tab(10).unwrap().title, "vim");
        assert_eq!(held.read().revision, 43);

        assert!(
            !shared.apply_batch(&batch(43, vec![])),
            "the fence holds through the handle"
        );

        // A resync replaces the contents, not the handle: everything
        // already holding it sees the rebuild.
        shared.reset(HostMirror::from_list(
            TabListResult {
                projects: vec![project(9, vec![])],
                revision: Some(60),
            },
            60,
        ));
        assert_eq!(held.read().revision, 60);
        assert!(held.read().tab(10).is_none());
    }

    /// Schema drift is a log line, not a panic and not a half-applied
    /// event.
    #[test]
    fn an_undecodable_payload_is_ignored() {
        let mut mirror = mirror();
        let before = mirror.clone();
        mirror.apply_event(&event(
            ops::EVENT_TAB_TITLE_CHANGED,
            serde_json::json!({"tab_id": "not-a-number"}),
        ));
        assert_eq!(mirror, before);
    }
}
