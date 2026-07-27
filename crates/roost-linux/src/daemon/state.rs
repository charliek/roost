//! In-process workspace state.
//!
//! Daemon-removal refactor M3 — rewritten from
//! `crates/roost-core/src/state.rs`. Differences vs the legacy
//! daemon original:
//!
//! * **Storage**: an in-memory `BTreeMap` instead of the SQLite
//!   `Store`. State is persisted to `state.json` (atomic write +
//!   one-level backup) via `store_json::persist_state`.
//! * **Types**: `roost_ipc::messages::{Tab, Project, TabState}`
//!   instead of the legacy `roost_proto::v1::*` types.
//! * **Events**: a typed `WorkspaceEvent` enum is emitted on a
//!   `tokio::sync::broadcast` channel. The IPC server's
//!   `events.subscribe` op (stubbed in M0; wired later) will convert
//!   these into `roost_ipc::messages::EventEnvelope`.
//! * **Session layout**: the workspace persists each project's tab
//!   layout (title + cwd + position) plus the active selection, so a
//!   relaunch re-opens the prior tabs as fresh shells in their saved
//!   directories. Live state (process, scrollback) is not restored.
//!   `open()` loads the layout into a one-shot `restore_layout` the UI
//!   bootstrap drains via `take_restore_layout`; it is kept out of the
//!   live `tabs` map (those are the re-opened fresh shells).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

use roost_ipc::agent::{
    self, AgentLifecycle, AgentTabState, AttentionEffect, OwnershipAction, TabAgentReportParams,
};
use roost_ipc::messages::{Project, Tab, TabState};
use tokio::sync::broadcast;
use tracing::warn;

use crate::daemon::store_json::{persist_state, read_state, SnapshotFile};

/// How many events the broadcast channel buffers per subscriber.
/// Subscribers that fall behind get a `Lagged` and resync via
/// `tab.list`.
const EVENT_CHANNEL_CAPACITY: usize = 256;

#[derive(Clone, Debug)]
struct ProjectRow {
    id: i64,
    name: String,
    cwd: String,
    position: i32,
    created_at: i64,
}

#[derive(Clone, Debug)]
struct TabRow {
    id: i64,
    project_id: i64,
    title: String,
    cwd: String,
    /// The three agent axes. `Tab.state` and `Tab.hook_active` are
    /// derived from this on the way out (`wire_tab`), never stored
    /// beside it.
    ///
    /// **Not persisted.** `TabSnapshot` stays `{title, cwd, position,
    /// user_titled}`; run state has never been in `state.json` and this
    /// plan does not put it there (plan 002 §2.6).
    agent: AgentTabState,
    has_notification: bool,
    user_titled: bool,
    position: i32,
    created_at: i64,
    last_active: i64,
}

struct Inner {
    projects: BTreeMap<i64, ProjectRow>,
    tabs: BTreeMap<i64, TabRow>,
    next_id: i64,
    active_project_id: i64,
    active_tab_id: i64,
    /// Whether the sidebar is collapsed (hidden). UI-set via
    /// `set_sidebar_collapsed`; persisted so a relaunch restores it
    /// (GTK parity with the Mac UI's `RoostSidebarVisible`).
    sidebar_collapsed: bool,
    /// Whether the UI window currently has focus. Half of the
    /// notification-suppression predicate (plan §3.5); reported by the
    /// UI via [`Workspace::set_window_focused`]. Never persisted — focus
    /// is a property of the running session, not of the layout.
    window_focused: bool,
    /// Monotonic commit counter, bumped each time a persistable
    /// snapshot is taken (under this lock). Tags each snapshot so
    /// `persist()` can drop stale out-of-order writes (#80).
    persist_seq: u64,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            projects: BTreeMap::new(),
            tabs: BTreeMap::new(),
            next_id: 0,
            active_project_id: 0,
            active_tab_id: 0,
            sidebar_collapsed: false,
            // Focused until a UI says otherwise: a headless or IPC-only
            // workspace never reports focus, and the alternative default
            // would leave the active tab permanently "unseen" — silently
            // routing every notification the opposite way from what a
            // real window would.
            window_focused: true,
            persist_seq: 0,
        }
    }
}

/// A persisted project's tab layout, surfaced to the UI bootstrap.
/// These are descriptors (cwd + title), not live tabs — the UI
/// re-opens them as fresh shells via the normal open path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreLayout {
    pub projects: Vec<RestoreProject>,
    /// Project to re-select (`0` = no preference → first project).
    pub active_project_id: i64,
    /// Position of the active tab within the active project.
    pub active_tab_position: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreProject {
    pub project_id: i64,
    /// Tabs in display (position) order.
    pub tabs: Vec<RestoreTab>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreTab {
    pub cwd: String,
    pub title: String,
    /// Whether the saved title was a manual user rename (Cmd+R /
    /// `tab.set_title`). When true, the restore path re-asserts
    /// the `user_titled` lock so a post-relaunch `cd` doesn't
    /// silently re-derive the title from cwd. See `set_tab_cwd`
    /// for the gate. Persisted in `TabSnapshot.user_titled`.
    pub user_titled: bool,
}

/// Workspace event channel. Server-push subscribers in `ipc.rs`
/// convert these to wire-format `EventEnvelope`s.
#[derive(Debug, Clone)]
pub enum WorkspaceEvent {
    TabOpened(Tab),
    TabClosed {
        tab_id: i64,
    },
    TabStateChanged {
        tab_id: i64,
        state: TabState,
    },
    TabTitleChanged {
        tab_id: i64,
        title: String,
    },
    TabCwdChanged {
        tab_id: i64,
        cwd: String,
    },
    TabNotification {
        tab_id: i64,
        has_pending: bool,
    },
    ProjectCreated(Project),
    ProjectRenamed {
        project_id: i64,
        name: String,
    },
    ProjectDeleted {
        project_id: i64,
    },
    ActiveChanged {
        project_id: i64,
        tab_id: i64,
    },
    HookActiveChanged {
        tab_id: i64,
        active: bool,
    },
    /// The full agent record after an accepted report or shell mark;
    /// see [`roost_ipc::messages::AgentReportChangedEvent`].
    AgentChanged {
        tab_id: i64,
        agent: AgentTabState,
    },
    NotificationFired {
        tab_id: i64,
        title: String,
        body: String,
    },
    /// Fired after `reorder_tabs`. `tab_ids` is the post-reorder
    /// display order — the supplied prefix followed by any
    /// unlisted siblings in their prior position order. Mirrors
    /// the Mac side's `Workspace.Event.tabsReordered`.
    TabsReordered {
        project_id: i64,
        tab_ids: Vec<i64>,
    },
    /// Fired after `reorder_projects`. `project_ids` is the
    /// post-reorder sidebar order.
    ProjectsReordered {
        project_ids: Vec<i64>,
    },
    /// Full-state recovery snapshot. Minted by the event bridge
    /// (`events::subscribe`) when the broadcast channel reports
    /// `Lagged`, so the UI reconciles against ground truth instead
    /// of applying deltas on top of a diverged base. Each `Project`
    /// carries its live tabs; the active tab is the one with
    /// `is_active == true`.
    Resync(Vec<Project>),
}

pub struct Workspace {
    inner: Mutex<Inner>,
    /// Workspace event channel. Mutators publish on this **while
    /// still holding `inner`**, so broadcast order matches commit
    /// order: a fast subscriber must never observe an event sequence
    /// that contradicts the committed state (e.g. `TabClosed` before
    /// the `TabOpened` of the same tab when two mutators race). #80.
    /// `broadcast::Sender::send` is synchronous and non-blocking — it
    /// wakes receivers but never runs them inline — so holding the
    /// std `Mutex` across it cannot deadlock. Durability
    /// (`persist`) deliberately runs *after* the lock drops.
    events: broadcast::Sender<WorkspaceEvent>,
    /// Where to write the `state.json` file. `None` means the
    /// in-memory variant (used by tests).
    state_path: Option<PathBuf>,
    /// Guards `state.json` writes and tracks the highest commit seq
    /// already persisted. `persist()` serializes on this and skips
    /// any snapshot older than what's on disk, so a slow earlier
    /// commit can't clobber a newer one when writes race. The seq is
    /// assigned under `inner`, so it reflects commit order (#80).
    persist_guard: Mutex<u64>,
    /// One-shot tab layout loaded from `state.json` at `open` time,
    /// awaiting hydration by the UI bootstrap (`take_restore_layout`).
    /// `None` for the in-memory variant and after it's taken. Kept
    /// out of `inner.tabs` — the live tabs are the fresh shells the
    /// UI re-opens from these descriptors.
    restore_layout: Mutex<Option<RestoreLayout>>,
    /// Set by `flush()` on clean exit, *after* it writes the final
    /// layout. Once set, `persist()` is a no-op so a teardown-induced
    /// PTY-exit cascade (the window closing kills its shells) can't
    /// race in and overwrite the flushed layout with an empty one.
    /// Lock-free because `persist()` runs after the `inner` lock drops
    /// — it can't read a field guarded by that lock.
    shutting_down: AtomicBool,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("project {0} not found")]
    ProjectNotFound(i64),
    #[error("tab {0} not found")]
    TabNotFound(i64),
    #[error("tab {tab_id} does not belong to project {project_id}")]
    TabProjectMismatch { project_id: i64, tab_id: i64 },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde_json: {0}")]
    Json(#[from] serde_json::Error),
}

impl Workspace {
    /// Construct an empty in-memory workspace. Used by tests.
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            inner: Mutex::new(Inner::default()),
            events: tx,
            state_path: None,
            persist_guard: Mutex::new(0),
            restore_layout: Mutex::new(None),
            shutting_down: AtomicBool::new(false),
        }
    }

    /// Construct a workspace backed by `state_path`. Loads the file
    /// if present; corrupt or absent → empty workspace (warn-log).
    pub fn open(state_path: PathBuf) -> Self {
        let snapshot = match read_state(&state_path) {
            Ok(Some(s)) => s,
            Ok(None) => SnapshotFile::default(),
            Err(err) => {
                warn!(
                    path = %state_path.display(),
                    ?err,
                    "state.json failed to load; starting empty"
                );
                SnapshotFile::default()
            }
        };
        let (tx, _rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let mut inner = Inner {
            next_id: snapshot.next_id.max(1),
            sidebar_collapsed: snapshot.sidebar_collapsed,
            ..Inner::default()
        };

        // Build the one-shot restore layout (tab descriptors) BEFORE
        // moving the projects into `inner`. These are NOT inserted as
        // live tabs — the UI bootstrap re-opens them as fresh shells
        // via `take_restore_layout` + the normal open path.
        let restore = RestoreLayout {
            active_project_id: snapshot.active_project_id,
            active_tab_position: snapshot.active_tab_position,
            projects: snapshot
                .projects
                .iter()
                .map(|p| {
                    let mut tabs: Vec<(i32, RestoreTab)> = p
                        .tabs
                        .iter()
                        .map(|t| {
                            (
                                t.position,
                                RestoreTab {
                                    cwd: t.cwd.clone(),
                                    title: t.title.clone(),
                                    user_titled: t.user_titled,
                                },
                            )
                        })
                        .collect();
                    tabs.sort_by_key(|(pos, _)| *pos);
                    RestoreProject {
                        project_id: p.id,
                        tabs: tabs.into_iter().map(|(_, t)| t).collect(),
                    }
                })
                .collect(),
        };

        for p in snapshot.projects {
            inner.projects.insert(
                p.id,
                ProjectRow {
                    id: p.id,
                    name: p.name,
                    cwd: p.cwd,
                    position: p.position,
                    created_at: p.created_at,
                },
            );
        }
        Self {
            inner: Mutex::new(inner),
            events: tx,
            state_path: Some(state_path),
            persist_guard: Mutex::new(0),
            restore_layout: Mutex::new(Some(restore)),
            shutting_down: AtomicBool::new(false),
        }
    }

    /// Take the one-shot tab layout loaded from `state.json` at
    /// `open` time. Returns `None` for the in-memory variant and on
    /// every call after the first. The UI bootstrap calls this once
    /// to re-open each project's saved tabs as fresh shells.
    pub fn take_restore_layout(&self) -> Option<RestoreLayout> {
        self.restore_layout.lock().unwrap().take()
    }

    /// The sidebar's persisted collapsed state. The UI reads this at
    /// startup to restore the user's hide/show choice (GTK parity with
    /// the Mac UI's `RoostSidebarVisible`).
    pub fn sidebar_collapsed(&self) -> bool {
        self.inner.lock().unwrap().sidebar_collapsed
    }

    /// Record the sidebar's collapsed state and persist it. Emits no
    /// event — the UI that toggled already flipped its own widget; this
    /// only writes the choice through so a relaunch restores it. A no-op
    /// (no write) when unchanged, so re-toggling to the same state can't
    /// churn `state.json`.
    pub fn set_sidebar_collapsed(&self, collapsed: bool) {
        let mut inner = self.inner.lock().unwrap();
        if inner.sidebar_collapsed == collapsed {
            return;
        }
        inner.sidebar_collapsed = collapsed;
        self.commit(inner, Vec::new(), Persist::Write);
    }

    /// Report the UI window's focus state. Half of the notification
    /// suppression predicate (plan §3.5); the other half — which tab is
    /// active — the workspace already owns, so [`raise_attention`] can
    /// decide suppression atomically instead of the UI re-deriving it
    /// after the fact.
    ///
    /// [`raise_attention`]: Workspace::raise_attention
    pub fn set_window_focused(&self, focused: bool) {
        self.inner.lock().unwrap().window_focused = focused;
    }

    pub fn subscribe(&self) -> broadcast::Receiver<WorkspaceEvent> {
        self.events.subscribe()
    }

    /// Snapshot of the workspace as it appears on the wire.
    pub fn snapshot(&self) -> Vec<Project> {
        let inner = self.inner.lock().unwrap();
        let mut out: Vec<Project> = inner
            .projects
            .values()
            .map(|p| Project {
                id: p.id,
                name: p.name.clone(),
                cwd: p.cwd.clone(),
                position: p.position,
                created_at: p.created_at,
                tabs: inner
                    .tabs
                    .values()
                    .filter(|t| t.project_id == p.id)
                    .map(|t| self.to_wire_tab(t, &inner))
                    .collect(),
            })
            .collect();
        out.sort_by_key(|p| (p.position, p.id));
        for p in &mut out {
            p.tabs.sort_by_key(|t| (t.position, t.id));
        }
        out
    }

    /// Build a `Resync` event carrying the current full snapshot.
    /// The event bridge sends this on broadcast `Lagged`.
    pub fn resync_event(&self) -> WorkspaceEvent {
        WorkspaceEvent::Resync(self.snapshot())
    }

    pub fn active(&self) -> (i64, i64) {
        let inner = self.inner.lock().unwrap();
        (inner.active_project_id, inner.active_tab_id)
    }

    /// Ensure a default project exists; return its id. Used by
    /// `tab.open` when the client passes `project_id = 0`.
    pub fn ensure_default_project(&self, cwd: &str) -> i64 {
        let mut inner = self.inner.lock().unwrap();
        if let Some(p) = inner.projects.values().next() {
            let id = p.id;
            let mut events = Vec::new();
            if inner.active_project_id == 0 {
                inner.active_project_id = id;
                events.push(WorkspaceEvent::ActiveChanged {
                    project_id: inner.active_project_id,
                    tab_id: inner.active_tab_id,
                });
            }
            // No inline write here (as before): the only mutation is
            // the active selection, which `flush()` captures on exit.
            self.commit(inner, events, Persist::Skip);
            return id;
        }
        let id = inner.alloc_id();
        let position = inner.next_project_position();
        let now = unix_now();
        inner.projects.insert(
            id,
            ProjectRow {
                id,
                name: "Default".into(),
                cwd: cwd.to_string(),
                position,
                created_at: now,
            },
        );
        inner.active_project_id = id;
        let project = Project {
            id,
            name: "Default".into(),
            cwd: cwd.to_string(),
            position,
            created_at: now,
            tabs: vec![],
        };
        let events = vec![
            WorkspaceEvent::ProjectCreated(project),
            WorkspaceEvent::ActiveChanged {
                project_id: id,
                tab_id: 0,
            },
        ];
        self.commit(inner, events, Persist::Write);
        id
    }

    pub fn create_project(&self, name: &str, cwd: &str) -> Result<Project, WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let id = inner.alloc_id();
        let position = inner.next_project_position();
        let chosen_name = if name.is_empty() {
            format!("Untitled {}", inner.projects.len() + 1)
        } else {
            name.to_string()
        };
        let row = ProjectRow {
            id,
            name: chosen_name,
            cwd: cwd.to_string(),
            position,
            created_at: unix_now(),
        };
        inner.projects.insert(id, row.clone());

        let project = Project {
            id: row.id,
            name: row.name,
            cwd: row.cwd,
            position: row.position,
            created_at: row.created_at,
            tabs: vec![],
        };
        self.commit(
            inner,
            vec![WorkspaceEvent::ProjectCreated(project.clone())],
            Persist::Write,
        );
        Ok(project)
    }

    pub fn rename_project(&self, project_id: i64, name: &str) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let row = inner
            .projects
            .get_mut(&project_id)
            .ok_or(WorkspaceError::ProjectNotFound(project_id))?;
        row.name = name.to_string();
        self.commit(
            inner,
            vec![WorkspaceEvent::ProjectRenamed {
                project_id,
                name: name.to_string(),
            }],
            Persist::Write,
        );
        Ok(())
    }

    /// Delete a project. Cascades to its tabs (per-tab
    /// `TabClosed` events emitted first, then `ProjectDeleted`,
    /// then `ActiveChanged` if the selection moved). PTY cleanup
    /// is the caller's responsibility — the workspace doesn't own
    /// a `PtySupervisor` reference.
    pub fn delete_project(&self, project_id: i64) -> Result<Vec<i64>, WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.projects.contains_key(&project_id) {
            return Err(WorkspaceError::ProjectNotFound(project_id));
        }
        let tab_ids: Vec<i64> = inner
            .tabs
            .values()
            .filter(|t| t.project_id == project_id)
            .map(|t| t.id)
            .collect();
        for tid in &tab_ids {
            inner.tabs.remove(tid);
        }
        inner.projects.remove(&project_id);

        // Adjust active selection if it points at the deleted
        // project or one of its tabs.
        let mut active_changed = false;
        if inner.active_project_id == project_id || tab_ids.contains(&inner.active_tab_id) {
            let fallback_project = inner.projects.keys().next().copied().unwrap_or(0);
            let fallback_tab = inner
                .tabs
                .values()
                .find(|t| t.project_id == fallback_project)
                .map(|t| t.id)
                .unwrap_or(0);
            inner.active_project_id = fallback_project;
            inner.active_tab_id = fallback_tab;
            active_changed = true;
        }
        let active = if active_changed {
            Some((inner.active_project_id, inner.active_tab_id))
        } else {
            None
        };

        // Commit order: TabClosed* → ProjectDeleted → ActiveChanged.
        let mut events: Vec<WorkspaceEvent> = tab_ids
            .iter()
            .map(|tid| WorkspaceEvent::TabClosed { tab_id: *tid })
            .collect();
        events.push(WorkspaceEvent::ProjectDeleted { project_id });
        if let Some((pid, tid)) = active {
            events.push(WorkspaceEvent::ActiveChanged {
                project_id: pid,
                tab_id: tid,
            });
        }
        self.commit(inner, events, Persist::Write);
        Ok(tab_ids)
    }

    /// Open a new tab in `project_id`. Returns the wire-format
    /// `Tab`. Caller spawns the PTY.
    pub fn open_tab(&self, project_id: i64, cwd: &str, title: &str) -> Result<Tab, WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.projects.contains_key(&project_id) {
            return Err(WorkspaceError::ProjectNotFound(project_id));
        }
        let id = inner.alloc_id();
        let now = unix_now();
        let position = inner.next_tab_position(project_id);
        let derived_title = if title.is_empty() {
            derive_title(cwd)
        } else {
            title.to_string()
        };
        let row = TabRow {
            id,
            project_id,
            title: derived_title.clone(),
            cwd: cwd.to_string(),
            agent: AgentTabState::default(),
            has_notification: false,
            // Always start with user_titled=false. The caller-
            // supplied `title` is a placeholder (e.g. UI's
            // "roost-mac N" / CLI's "roostctl" default) that
            // shell-side OSC 0/1/2 emissions should be allowed to
            // overwrite. Only an explicit user rename via
            // `set_tab_title` flips this to true. The previous
            // `!title.is_empty()` policy locked every newly-opened
            // tab to its placeholder, preventing shell prompts
            // like `👻 /tmp` from ever appearing in the tab bar.
            // Mirrors the Mac fix in `mac/Sources/Roost/Workspace.swift`.
            user_titled: false,
            position,
            created_at: now,
            last_active: now,
        };
        inner.tabs.insert(id, row.clone());
        // New tabs steal the active selection.
        inner.active_project_id = project_id;
        inner.active_tab_id = id;

        let tab = self.to_wire_tab(&row, &inner);
        self.commit(
            inner,
            vec![
                WorkspaceEvent::TabOpened(tab.clone()),
                WorkspaceEvent::ActiveChanged {
                    project_id,
                    tab_id: id,
                },
            ],
            Persist::Write,
        );
        Ok(tab)
    }

    /// Close a tab. If it was the project's **last** tab, the
    /// project is closed too (mirrors `delete_project`'s cascade) so
    /// a project can never linger with zero live tabs. The event
    /// order in that case is `TabClosed → ProjectDeleted →
    /// ActiveChanged`, matching `delete_project`; both UIs already
    /// converge on `ProjectDeleted` (remove the sidebar row, pick a
    /// fallback project, or close the window when none remain).
    pub fn close_tab(&self, tab_id: i64) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let row = inner
            .tabs
            .remove(&tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;
        let project_id = row.project_id;

        // Last tab in the project? Cascade-close the project. Inlined
        // rather than calling `delete_project` so the event order is
        // exactly TabClosed → ProjectDeleted → ActiveChanged (the tab
        // is already removed; `delete_project` would re-emit it).
        let project_emptied = inner.projects.contains_key(&project_id)
            && !inner.tabs.values().any(|t| t.project_id == project_id);
        if project_emptied {
            inner.projects.remove(&project_id);
        }

        // Reassign the active selection if it pointed at the closed
        // tab (or, when the project went away, at that project).
        let mut changed = false;
        if inner.active_tab_id == tab_id
            || (project_emptied && inner.active_project_id == project_id)
        {
            let next = if project_emptied {
                // Project gone: fall back to another project's tab.
                let fallback_project = inner.projects.keys().next().copied().unwrap_or(0);
                let fallback_tab = inner
                    .tabs
                    .values()
                    .find(|t| t.project_id == fallback_project)
                    .map(|t| t.id)
                    .unwrap_or(0);
                (fallback_project, fallback_tab)
            } else {
                // Project survives: fall back to a sibling tab, else
                // any tab anywhere.
                inner
                    .tabs
                    .values()
                    .find(|t| t.project_id == project_id)
                    .or_else(|| inner.tabs.values().next())
                    .map(|t| (t.project_id, t.id))
                    .unwrap_or((project_id, 0))
            };
            inner.active_project_id = next.0;
            inner.active_tab_id = next.1;
            changed = true;
        }
        let active = if changed {
            Some((inner.active_project_id, inner.active_tab_id))
        } else {
            None
        };

        // Commit order: TabClosed → ProjectDeleted? → ActiveChanged?.
        let mut events = vec![WorkspaceEvent::TabClosed { tab_id }];
        if project_emptied {
            events.push(WorkspaceEvent::ProjectDeleted { project_id });
        }
        if let Some((pid, tid)) = active {
            events.push(WorkspaceEvent::ActiveChanged {
                project_id: pid,
                tab_id: tid,
            });
        }
        self.commit(inner, events, Persist::Write);
        Ok(())
    }

    pub fn set_tab_title(&self, tab_id: i64, title: &str) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let row = inner
            .tabs
            .get_mut(&tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;
        row.title = title.to_string();
        row.user_titled = true;
        self.commit(
            inner,
            vec![WorkspaceEvent::TabTitleChanged {
                tab_id,
                title: title.to_string(),
            }],
            Persist::Write,
        );
        Ok(())
    }

    /// OSC 0/1/2 paths set the title only if the user hasn't
    /// manually renamed the tab.
    pub fn set_tab_title_from_osc(&self, tab_id: i64, title: &str) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let row = inner
            .tabs
            .get_mut(&tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;
        if row.user_titled {
            return Ok(());
        }
        row.title = title.to_string();
        // Shell-driven (OSC titles fire per prompt) but the write is
        // cheap now (no fsync until flush()), so write through.
        self.commit(
            inner,
            vec![WorkspaceEvent::TabTitleChanged {
                tab_id,
                title: title.to_string(),
            }],
            Persist::Write,
        );
        Ok(())
    }

    pub fn set_tab_cwd(&self, tab_id: i64, cwd: &str) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let row = inner
            .tabs
            .get_mut(&tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;
        let cwd_owned = cwd.to_string();
        row.cwd = cwd_owned.clone();
        // Shell-driven (OSC 7 fires per `cd`) but write-through: each
        // change lands in the page cache without an fsync, so a `cd`
        // loop is cheap and the latest cwd is always on disk.
        let mut events = vec![WorkspaceEvent::TabCwdChanged {
            tab_id,
            cwd: cwd_owned,
        }];
        // Re-derive title from cwd when the user hasn't explicitly
        // renamed (mirrors `set_tab_title_from_osc`'s `user_titled`
        // gate). Lets the title follow cwd on shells without
        // integration (Apple /bin/bash 3.2, --norc bash). On integrated
        // shells, the next prompt's OSC 0 refines this basename to the
        // tilde-abbreviated path via `set_tab_title_from_osc` —
        // latest-wins. Event order is cwd-then-title (cause-then-effect).
        if !row.user_titled {
            let new_title = derive_title(cwd);
            if row.title != new_title {
                row.title = new_title.clone();
                events.push(WorkspaceEvent::TabTitleChanged {
                    tab_id,
                    title: new_title,
                });
            }
        }
        self.commit(inner, events, Persist::Write);
        Ok(())
    }

    /// Raise a tab's attention — pending badge, inbox row, and desktop
    /// banner — as **one transaction**. Returns whether it was delivered.
    ///
    /// Every gate lives here, under the same lock as the mutation,
    /// because each is a race otherwise. Reading the agent gate and then
    /// committing lets a concurrent `tab.agent_report` claim the tab in
    /// between; leaving focus to the UI's event drain re-evaluates it
    /// against a selection the user may have changed since, which both
    /// loses notifications and resurrects them retroactively.
    ///
    /// A suppressed raise is dropped **entirely** — no pending bit, no
    /// events. That is what makes plan §3.5's "switching away afterwards
    /// does not retroactively produce a badge" true by construction
    /// rather than by a UI write-back.
    pub fn raise_attention(
        &self,
        tab_id: i64,
        title: &str,
        body: &str,
        source: AttentionSource,
    ) -> Result<bool, WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let focus_suppressed = inner.attention_suppressed_by_focus(tab_id);
        // A 'lookup' error rather than a silent drop: hook tools racing
        // a tab close need to tell "gone" from "suppressed".
        let row = inner
            .tabs
            .get_mut(&tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;
        // Only *raw* OSC is gated on ownership. A structured
        // `notification.create` / `roostctl notify` is never suppressed
        // this way (plan §3.4) — the agent's own adapter is what emits
        // those, so gating them would mute the very source it protects.
        if source == AttentionSource::RawOsc && agent::suppress_raw_osc(&row.agent) {
            tracing::debug!(tab_id, "raw OSC notification suppressed (agent owns tab)");
            return Ok(false);
        }
        if focus_suppressed {
            return Ok(false);
        }
        row.has_notification = true;
        // Notification state isn't in the persisted snapshot — emit only.
        self.commit(
            inner,
            vec![
                WorkspaceEvent::TabNotification {
                    tab_id,
                    has_pending: true,
                },
                WorkspaceEvent::NotificationFired {
                    tab_id,
                    title: title.to_string(),
                    body: body.to_string(),
                },
            ],
            Persist::Skip,
        );
        Ok(true)
    }

    /// `tab.agent_report` — the one op every agent adapter writes
    /// through. Session scoping, ownership supersede, and the patch
    /// semantics all live in [`agent::apply_report`]; this method owns
    /// only the mutation and the event fan-out.
    ///
    /// Returns the post-report tab plus whether the report was accepted
    /// (false = dropped on an ownership mismatch, tab unchanged).
    pub fn agent_report(
        &self,
        report: &TabAgentReportParams,
    ) -> Result<(bool, Tab), WorkspaceError> {
        self.apply_agent_reports(report, None)
    }

    /// Legacy `tab.set_state`, re-expressed on the agent axis per plan
    /// §3.7. Each value claims ownership as `manual` (an empty session
    /// id) so the user's override supersedes a live agent — that is
    /// what taking the wheel means.
    ///
    /// `none` additionally releases: "no state" most closely means "no
    /// agent owns this tab", so derivation falls back to the shell axis
    /// (`none` at a prompt, `running` under a live foreground process).
    /// It claims *before* releasing because a bare release is scoped to
    /// the current owner — claiming first is the only path that can
    /// take the tab from someone else, and it keeps the matching rule
    /// inside `agent::apply_report` rather than duplicated here.
    pub fn set_tab_state(&self, tab_id: i64, state: TabState) -> Result<(), WorkspaceError> {
        let lifecycle = match state {
            TabState::Running => AgentLifecycle::Working,
            TabState::NeedsInput => AgentLifecycle::Waiting,
            TabState::Idle => AgentLifecycle::Finished,
            TabState::None => AgentLifecycle::Inactive,
        };
        let claim = TabAgentReportParams::sessionless(
            tab_id,
            SOURCE_MANUAL,
            OwnershipAction::Claim,
            Some(lifecycle),
        );
        let release = (state == TabState::None).then(|| {
            TabAgentReportParams::sessionless(tab_id, SOURCE_MANUAL, OwnershipAction::Release, None)
        });
        self.apply_agent_reports(&claim, release.as_ref())
            .map(|_| ())
    }

    /// Deprecated `tab.set_hook_active` alias (plan §3.6): claim or
    /// release as `legacy` with an empty session id. Release is scoped
    /// like any other, so it cannot revoke a real agent's ownership.
    pub fn set_tab_hook_active(&self, tab_id: i64, active: bool) -> Result<(), WorkspaceError> {
        let action = if active {
            OwnershipAction::Claim
        } else {
            OwnershipAction::Release
        };
        let report = TabAgentReportParams::sessionless(tab_id, SOURCE_LEGACY, action, None);
        self.apply_agent_reports(&report, None).map(|_| ())
    }

    /// OSC 133 prompt/command mark → the **shell** axis, ungated.
    ///
    /// The old `hook_active` gate is gone: the axes are independent, so
    /// there is nothing to suppress — derivation decides which one
    /// shows. `A`/`B`/`D` additionally drop the lifecycle to `inactive`
    /// (keeping ownership as a label), which is the failsafe against a
    /// killed agent muting the tab forever. See [`agent::apply_shell_mark`].
    pub fn apply_shell_mark(&self, tab_id: i64, body: &str) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let row = inner
            .tabs
            .get_mut(&tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;
        let Some(next) = agent::apply_shell_mark(&row.agent, body) else {
            return Ok(()); // undefined mark body — no change
        };
        let events = replace_agent(row, next);
        // Run state isn't in the persisted snapshot — emit only.
        self.commit(inner, events, Persist::Skip);
        Ok(())
    }

    /// The tab's PTY was replaced: the shell that hosted any agent is
    /// gone, so both axes and ownership reset.
    ///
    /// Stated as a rule about the PTY rather than about closing —
    /// closing drops the whole row, so it needs no help. #170's
    /// hard-restart keeps the row and is this call.
    pub fn pty_replaced(&self, tab_id: i64) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let row = inner
            .tabs
            .get_mut(&tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;
        let events = replace_agent(row, AgentTabState::default());
        self.commit(inner, events, Persist::Skip);
        Ok(())
    }

    /// Apply `first` and then `then` under one lock, emitting the
    /// derived-state deltas once for the net result.
    ///
    /// The second slot exists for exactly one caller — `set-state none`,
    /// a claim followed by a release. Applying the pair under one lock
    /// keeps it atomic and stops the intermediate claim from reaching
    /// subscribers as a real state.
    fn apply_agent_reports(
        &self,
        first: &TabAgentReportParams,
        then: Option<&TabAgentReportParams>,
    ) -> Result<(bool, Tab), WorkspaceError> {
        let tab_id = first.tab_id;
        let now = unix_now();
        let mut inner = self.inner.lock().unwrap();
        let is_active = inner.active_tab_id == tab_id;
        let focus_suppressed = inner.attention_suppressed_by_focus(tab_id);
        let row = inner
            .tabs
            .get_mut(&tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;

        let mut next = row.agent.clone();
        let mut accepted = false;
        let mut attention = AttentionEffect::Unchanged;
        for report in std::iter::once(first).chain(then) {
            let outcome = agent::apply_report(&next, report, now);
            if !outcome.accepted {
                continue;
            }
            accepted = true;
            next = outcome.state;
            if outcome.attention != AttentionEffect::Unchanged {
                attention = outcome.attention;
            }
        }

        let mut events = replace_agent(row, next);
        match attention {
            // Same focus predicate as `raise_attention`, applied here
            // rather than by delegating because the report's own
            // mutation must stay in this transaction: a Claude
            // notification for the tab you are looking at is suppressed
            // exactly like any other, but its lifecycle change is not.
            //
            // `severity` stops here in v1 by design: policy B (plan
            // §3.5) suppresses on focus alone, and "failed overrides
            // suppression" is a later slice. It stays readable on the
            // report itself rather than being plumbed through an event
            // nobody reads yet.
            AttentionEffect::Set {
                title,
                body,
                severity: _,
            } if !focus_suppressed => {
                row.has_notification = true;
                events.push(WorkspaceEvent::TabNotification {
                    tab_id,
                    has_pending: true,
                });
                events.push(WorkspaceEvent::NotificationFired {
                    tab_id,
                    title,
                    body,
                });
            }
            AttentionEffect::Set { .. } => {}
            AttentionEffect::Clear => {
                row.has_notification = false;
                events.push(WorkspaceEvent::TabNotification {
                    tab_id,
                    has_pending: false,
                });
            }
            AttentionEffect::Unchanged => {}
        }
        let tab = wire_tab(row, is_active);
        // Run state isn't in the persisted snapshot — emit only.
        self.commit(inner, events, Persist::Skip);
        Ok((accepted, tab))
    }

    pub fn set_tab_has_notification(
        &self,
        tab_id: i64,
        has_pending: bool,
    ) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let row = inner
            .tabs
            .get_mut(&tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;
        row.has_notification = has_pending;
        // Notification flag isn't in the persisted snapshot — emit only.
        self.commit(
            inner,
            vec![WorkspaceEvent::TabNotification {
                tab_id,
                has_pending,
            }],
            Persist::Skip,
        );
        Ok(())
    }

    pub fn focus_tab(&self, tab_id: i64) -> Result<(i64, i64), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let row = inner
            .tabs
            .get(&tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?
            .clone();
        let prev = (inner.active_project_id, inner.active_tab_id);
        inner.active_project_id = row.project_id;
        inner.active_tab_id = row.id;
        // Persist the active selection so it survives a relaunch
        // (restored by position). Skip when unchanged — focusing the
        // already-active tab shouldn't churn the file.
        let persist = if prev != (row.project_id, row.id) {
            Persist::Write
        } else {
            Persist::Skip
        };
        self.commit(
            inner,
            vec![WorkspaceEvent::ActiveChanged {
                project_id: row.project_id,
                tab_id: row.id,
            }],
            persist,
        );
        Ok(prev)
    }

    pub fn reorder_tabs(&self, project_id: i64, tab_ids: &[i64]) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.projects.contains_key(&project_id) {
            return Err(WorkspaceError::ProjectNotFound(project_id));
        }
        // Validate all referenced tabs exist and belong to the project.
        for tid in tab_ids {
            let row = inner
                .tabs
                .get(tid)
                .ok_or(WorkspaceError::TabNotFound(*tid))?;
            if row.project_id != project_id {
                return Err(WorkspaceError::TabProjectMismatch {
                    project_id,
                    tab_id: *tid,
                });
            }
        }
        // Reassign positions in the order given, then keep any
        // tabs not listed at their relative trailing positions.
        let mut next_pos = 0i32;
        for tid in tab_ids {
            if let Some(row) = inner.tabs.get_mut(tid) {
                row.position = next_pos;
                next_pos += 1;
            }
        }
        // Tabs in the project that were not listed: append in
        // their existing order.
        let mut unlisted: Vec<i64> = inner
            .tabs
            .values()
            .filter(|t| t.project_id == project_id && !tab_ids.contains(&t.id))
            .map(|t| t.id)
            .collect();
        unlisted.sort_by_key(|tid| inner.tabs.get(tid).map(|r| r.position).unwrap_or(0));
        for tid in &unlisted {
            if let Some(row) = inner.tabs.get_mut(tid) {
                row.position = next_pos;
                next_pos += 1;
            }
        }
        // Compute the full post-reorder order for the event
        // payload: supplied prefix + sorted unlisted (matches
        // Mac's `Workspace.tabsReordered` payload shape).
        let final_order: Vec<i64> = tab_ids.iter().copied().chain(unlisted).collect();
        self.commit(
            inner,
            vec![WorkspaceEvent::TabsReordered {
                project_id,
                tab_ids: final_order,
            }],
            Persist::Write,
        );
        Ok(())
    }

    pub fn reorder_projects(&self, project_ids: &[i64]) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        for pid in project_ids {
            if !inner.projects.contains_key(pid) {
                return Err(WorkspaceError::ProjectNotFound(*pid));
            }
        }
        let mut next_pos = 0i32;
        for pid in project_ids {
            if let Some(row) = inner.projects.get_mut(pid) {
                row.position = next_pos;
                next_pos += 1;
            }
        }
        let mut unlisted: Vec<i64> = inner
            .projects
            .values()
            .filter(|p| !project_ids.contains(&p.id))
            .map(|p| p.id)
            .collect();
        unlisted.sort_by_key(|pid| inner.projects.get(pid).map(|r| r.position).unwrap_or(0));
        for pid in &unlisted {
            if let Some(row) = inner.projects.get_mut(pid) {
                row.position = next_pos;
                next_pos += 1;
            }
        }
        let final_order: Vec<i64> = project_ids.iter().copied().chain(unlisted).collect();
        self.commit(
            inner,
            vec![WorkspaceEvent::ProjectsReordered {
                project_ids: final_order,
            }],
            Persist::Write,
        );
        Ok(())
    }

    pub fn tab(&self, tab_id: i64) -> Result<Tab, WorkspaceError> {
        let inner = self.inner.lock().unwrap();
        let is_active = inner.active_tab_id == tab_id;
        let row = inner
            .tabs
            .get(&tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;
        Ok(wire_tab(row, is_active))
    }

    fn to_wire_tab(&self, row: &TabRow, inner: &Inner) -> Tab {
        wire_tab(row, inner.active_tab_id == row.id)
    }

    /// Persist `snapshot` to `state.json`, tagged with its commit
    /// `seq`. Runs synchronously on the caller's thread (the inner
    /// lock is already released; writes are small atomic renames).
    /// `sync` forces an `fsync` (the clean-exit `flush()` path);
    /// during the session it's `false` — write-through into the page
    /// cache, no disk barrier. `persist_guard` serializes concurrent
    /// writers and drops any snapshot older than the newest already
    /// on disk, so a slow earlier commit can never clobber a newer
    /// one (#80).
    fn persist(&self, seq: u64, snapshot: SnapshotFile, sync: bool) {
        // Frozen by `flush()` on clean exit: ignore any later write so
        // a teardown cascade can't overwrite the flushed layout.
        if self.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        let Some(path) = self.state_path.clone() else {
            return; // in-memory variant; no persistence
        };
        let mut last = self.persist_guard.lock().unwrap();
        if seq <= *last {
            return; // a newer commit already persisted; this write is stale
        }
        if let Err(err) = persist_state(&path, &snapshot, sync) {
            warn!(?err, "failed to persist state.json");
        }
        // Advance past this seq even on write failure: an older
        // snapshot must never win, and there is no retry of `seq`.
        *last = seq;
    }

    /// Persist the current layout with `fsync` and then freeze further
    /// persistence. Call once on a clean exit (each UI wires it into
    /// its app-quit hook). The `fsync` re-asserts physical durability
    /// at quit time — belt-and-suspenders, since the session's
    /// write-through already left the latest layout in the page cache,
    /// readable by a relaunch even without it. Setting `shutting_down`
    /// *after* the write means `flush`'s own `persist` isn't blocked
    /// while every subsequent one is, so a teardown-induced PTY-exit
    /// cascade can't clobber the flushed layout. Idempotent: a second
    /// call is a no-op (the freeze short-circuits its `persist`).
    pub fn flush(&self) {
        let (snapshot, seq) = {
            let mut inner = self.inner.lock().unwrap();
            inner.snapshot_for_persist()
        };
        self.persist(seq, snapshot, true);
        self.shutting_down.store(true, Ordering::Relaxed);
    }

    /// Centralize the mutate → emit → persist tail shared by every
    /// mutator (#80). Snapshots **under the lock** when `persist` is
    /// `Persist::Write` (so the seq reflects commit order), then sends
    /// every event **while still holding the lock** (broadcast order
    /// matches commit order — a fast subscriber can't observe a
    /// contradicting sequence), and only after dropping the lock does
    /// it write to disk (no I/O under the lock). `Persist::Skip` is
    /// for state that isn't part of the persisted snapshot (tab
    /// run-state, notification flags) — emit only.
    fn commit(
        &self,
        mut inner: MutexGuard<'_, Inner>,
        events: Vec<WorkspaceEvent>,
        persist: Persist,
    ) {
        let to_write = match persist {
            Persist::Skip => None,
            Persist::Write => Some(inner.snapshot_for_persist()),
        };
        for ev in events {
            let _ = self.events.send(ev);
        }
        drop(inner);
        if let Some((snapshot, seq)) = to_write {
            self.persist(seq, snapshot, false);
        }
    }
}

/// Where an attention-raise came from. The only behavioral difference
/// is the agent gate: raw OSC is dropped while a live agent owns the tab
/// (plan §3.4), a structured `notification.create` / `roostctl notify`
/// never is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttentionSource {
    RawOsc,
    Structured,
}

/// Whether a `commit()` should write `state.json`. `Write` for layout
/// changes (projects/tabs/order/active selection); `Skip` for state
/// that isn't in the persisted snapshot (the agent axes, notification
/// flags) — those emit an event but never touch disk.
enum Persist {
    Skip,
    Write,
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}

impl Inner {
    /// Plan §3.5's suppression predicate: a notification for the tab the
    /// user is actively looking at is considered seen, so it raises no
    /// banner, no badge, and no inbox row.
    fn attention_suppressed_by_focus(&self, tab_id: i64) -> bool {
        self.window_focused && self.active_tab_id == tab_id
    }

    fn alloc_id(&mut self) -> i64 {
        self.next_id = self.next_id.max(1) + 1;
        self.next_id
    }

    /// Next free project position: `max(position) + 1`, or 0 when
    /// empty. `len()` would collide after a delete-then-create
    /// because positions are sparse, not dense (#80).
    fn next_project_position(&self) -> i32 {
        self.projects
            .values()
            .map(|p| p.position)
            .max()
            .map_or(0, |m| m + 1)
    }

    /// Next free tab position within `project_id`: `max(position) + 1`,
    /// or 0 when the project has no tabs. See `next_project_position`.
    fn next_tab_position(&self, project_id: i64) -> i32 {
        self.tabs
            .values()
            .filter(|t| t.project_id == project_id)
            .map(|t| t.position)
            .max()
            .map_or(0, |m| m + 1)
    }

    /// Snapshot the persistable state plus a fresh commit sequence.
    /// The seq is assigned here — under the `inner` lock the caller
    /// holds — so it strictly reflects commit order; `persist()` uses
    /// it to drop stale out-of-order writes (#80). Each project
    /// carries its tab layout (title + cwd + position) so a relaunch
    /// can re-open the tabs in their saved directories.
    fn snapshot_for_persist(&mut self) -> (SnapshotFile, u64) {
        use crate::daemon::store_json::{ProjectSnapshot, TabSnapshot};
        self.persist_seq += 1;
        // Active tab restored by its DENSE index within the active
        // project's display-ordered tabs — not the raw `position`
        // field, which goes sparse after a mid-project close and
        // wouldn't match the re-opened tabs' contiguous 0..n indices
        // on restore (the UI selects the nth tab). #95 review.
        let active_tab_position = self
            .tabs
            .get(&self.active_tab_id)
            .map(|active| {
                let mut siblings: Vec<&TabRow> = self
                    .tabs
                    .values()
                    .filter(|t| t.project_id == active.project_id)
                    .collect();
                siblings.sort_by_key(|t| (t.position, t.id));
                siblings.iter().position(|t| t.id == active.id).unwrap_or(0) as i32
            })
            .unwrap_or(0);
        let snapshot = SnapshotFile {
            next_id: self.next_id,
            active_project_id: self.active_project_id,
            active_tab_position,
            sidebar_collapsed: self.sidebar_collapsed,
            projects: self
                .projects
                .values()
                .map(|p| {
                    let mut tabs: Vec<TabSnapshot> = self
                        .tabs
                        .values()
                        .filter(|t| t.project_id == p.id)
                        .map(|t| TabSnapshot {
                            title: t.title.clone(),
                            cwd: t.cwd.clone(),
                            position: t.position,
                            user_titled: t.user_titled,
                        })
                        .collect();
                    tabs.sort_by_key(|t| t.position);
                    ProjectSnapshot {
                        id: p.id,
                        name: p.name.clone(),
                        cwd: p.cwd.clone(),
                        position: p.position,
                        created_at: p.created_at,
                        tabs,
                    }
                })
                .collect(),
        };
        (snapshot, self.persist_seq)
    }
}

/// Ownership source for a hand-driven `roostctl tab set-state`.
const SOURCE_MANUAL: &str = "manual";
/// Ownership source for the deprecated `tab.set_hook_active` alias.
const SOURCE_LEGACY: &str = "legacy";

fn wire_tab(row: &TabRow, is_active: bool) -> Tab {
    Tab {
        id: row.id,
        project_id: row.project_id,
        title: row.title.clone(),
        cwd: row.cwd.clone(),
        state: agent::effective(&row.agent),
        has_notification: row.has_notification,
        is_active,
        user_titled: row.user_titled,
        position: row.position,
        created_at: row.created_at,
        last_active: row.last_active,
        hook_active: agent::is_live(&row.agent),
        shell_state: row.agent.shell,
        agent_lifecycle: row.agent.lifecycle,
        ownership: row.agent.ownership.clone(),
    }
}

/// Swap a tab's agent record and return the events the swap implies.
///
/// `AgentChanged` carries the full record; `TabStateChanged` and
/// `HookActiveChanged` carry its two derived projections and each fires
/// only when its own projection moved. An identical record emits
/// nothing — repeated prompt marks are the common case and should not
/// churn the UI.
fn replace_agent(row: &mut TabRow, next: AgentTabState) -> Vec<WorkspaceEvent> {
    if row.agent == next {
        return Vec::new();
    }
    let tab_id = row.id;
    let prev = std::mem::replace(&mut row.agent, next);
    let mut events = Vec::with_capacity(3);
    let state = agent::effective(&row.agent);
    if state != agent::effective(&prev) {
        events.push(WorkspaceEvent::TabStateChanged { tab_id, state });
    }
    let active = agent::is_live(&row.agent);
    if active != agent::is_live(&prev) {
        events.push(WorkspaceEvent::HookActiveChanged { tab_id, active });
    }
    events.push(WorkspaceEvent::AgentChanged {
        tab_id,
        agent: row.agent.clone(),
    });
    events
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn derive_title(cwd: &str) -> String {
    if cwd.is_empty() {
        return "shell".into();
    }
    // Special-case "/": Path::file_name() returns None for the root,
    // and the prior "shell" fallback diverges from the Swift twin
    // (NSString.lastPathComponent("/") returns "/") and from what
    // shell integration's __roost_title would emit ("/"). Return "/"
    // explicitly to keep both UIs in lockstep and avoid the surprising
    // "you're at root, but the tab title says 'shell'" UX on
    // un-integrated shells.
    if cwd == "/" {
        return "/".into();
    }
    std::path::Path::new(cwd)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "shell".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use roost_ipc::agent::{AttentionOp, ShellState};

    #[test]
    fn open_tab_emits_tab_opened() {
        let ws = Workspace::new();
        let pid = ws.create_project("p", "").unwrap().id;
        let mut rx = ws.subscribe();
        let _ = ws.open_tab(pid, "/", "").unwrap();
        // Two events fire: TabOpened + ActiveChanged. Pull both.
        let _first = rx.try_recv().expect("event one");
        let _second = rx.try_recv().expect("event two");
    }

    #[test]
    fn close_tab_falls_back_to_sibling() {
        let ws = Workspace::new();
        let pid = ws.create_project("p", "").unwrap().id;
        let t1 = ws.open_tab(pid, "/", "one").unwrap().id;
        let _t2 = ws.open_tab(pid, "/", "two").unwrap().id;
        let (apid_before, atid_before) = ws.active();
        assert_eq!(apid_before, pid);
        ws.close_tab(atid_before).unwrap();
        let (apid_after, atid_after) = ws.active();
        assert_eq!(apid_after, pid);
        // The remaining tab is now active. It's the one we did not close.
        assert_ne!(atid_after, atid_before);
        assert_eq!(atid_after, t1);
    }

    #[test]
    fn close_last_tab_deletes_project() {
        let ws = Workspace::new();
        let pid = ws.create_project("p", "").unwrap().id;
        let t = ws.open_tab(pid, "/", "only").unwrap().id;
        let mut rx = ws.subscribe();
        ws.close_tab(t).unwrap();
        // The project is gone with its last tab, so the only-project
        // workspace is now empty.
        assert!(ws.snapshot().is_empty());
        // Event order: TabClosed → ProjectDeleted → ActiveChanged.
        assert!(matches!(
            rx.try_recv(),
            Ok(WorkspaceEvent::TabClosed { tab_id }) if tab_id == t
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(WorkspaceEvent::ProjectDeleted { project_id }) if project_id == pid
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(WorkspaceEvent::ActiveChanged {
                project_id: 0,
                tab_id: 0
            })
        ));
        // Active selection cleared to (0, 0) since nothing remains.
        assert_eq!(ws.active(), (0, 0));
    }

    #[test]
    fn close_last_tab_of_inactive_project_keeps_active() {
        // Closing a non-active project's last tab deletes that project
        // but must not steal the active selection from elsewhere.
        let ws = Workspace::new();
        let a = ws.create_project("a", "").unwrap().id;
        let a_tab = ws.open_tab(a, "/", "a1").unwrap().id;
        let b = ws.create_project("b", "").unwrap().id;
        let b_tab = ws.open_tab(b, "/", "b1").unwrap().id;
        // Make project A active, then close project B's last tab.
        ws.focus_tab(a_tab).unwrap();
        ws.close_tab(b_tab).unwrap();
        let snap = ws.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].id, a);
        // Active stays on A; no spurious reassignment.
        assert_eq!(ws.active(), (a, a_tab));
    }

    #[test]
    fn delete_project_cascades_tabs() {
        let ws = Workspace::new();
        let pid = ws.create_project("p", "").unwrap().id;
        let _t1 = ws.open_tab(pid, "/", "one").unwrap();
        let _t2 = ws.open_tab(pid, "/", "two").unwrap();
        let deleted = ws.delete_project(pid).unwrap();
        assert_eq!(deleted.len(), 2);
        assert!(ws.snapshot().is_empty());
    }

    #[test]
    fn ensure_default_project_creates_only_once() {
        let ws = Workspace::new();
        let a = ws.ensure_default_project("/");
        let b = ws.ensure_default_project("/");
        assert_eq!(a, b);
    }

    #[test]
    fn set_tab_title_locks_against_osc() {
        let ws = Workspace::new();
        let pid = ws.create_project("p", "").unwrap().id;
        let tid = ws.open_tab(pid, "/", "").unwrap().id;
        ws.set_tab_title(tid, "manual").unwrap();
        ws.set_tab_title_from_osc(tid, "shell-says").unwrap();
        let t = ws.tab(tid).unwrap();
        assert_eq!(t.title, "manual");
        assert!(t.user_titled);
    }

    /// Issue #196: `set_tab_cwd` re-derives the tab title from cwd
    /// when `!user_titled`, so the title follows cwd on any shell
    /// (Apple bash 3.2 / `--norc` bash / etc.), not just shells with
    /// the OSC 0 integration loaded. Events fire cwd-then-title.
    #[test]
    fn set_tab_cwd_re_derives_title_when_not_user_titled() {
        let ws = Workspace::new();
        let pid = ws.create_project("p", "").unwrap().id;
        let tid = ws.open_tab(pid, "/tmp", "").unwrap().id;
        assert_eq!(ws.tab(tid).unwrap().title, "tmp");
        let mut rx = ws.subscribe();
        ws.set_tab_cwd(tid, "/usr").unwrap();
        // Cwd-then-title: cause-then-effect.
        assert!(matches!(
            rx.try_recv(),
            Ok(WorkspaceEvent::TabCwdChanged { tab_id, ref cwd })
                if tab_id == tid && cwd == "/usr"
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(WorkspaceEvent::TabTitleChanged { tab_id, ref title })
                if tab_id == tid && title == "usr"
        ));
        assert_eq!(ws.tab(tid).unwrap().title, "usr");
    }

    /// `set_tab_cwd` does NOT touch the title when the user has
    /// manually renamed (mirrors `set_tab_title_from_osc`'s gate).
    #[test]
    fn set_tab_cwd_preserves_user_titled_title() {
        let ws = Workspace::new();
        let pid = ws.create_project("p", "").unwrap().id;
        let tid = ws.open_tab(pid, "/tmp", "").unwrap().id;
        ws.set_tab_title(tid, "manual").unwrap();
        let mut rx = ws.subscribe();
        ws.set_tab_cwd(tid, "/usr").unwrap();
        assert!(matches!(
            rx.try_recv(),
            Ok(WorkspaceEvent::TabCwdChanged { tab_id, ref cwd })
                if tab_id == tid && cwd == "/usr"
        ));
        // No TabTitleChanged — user_titled blocks the re-derivation.
        assert!(rx.try_recv().is_err());
        assert_eq!(ws.tab(tid).unwrap().title, "manual");
    }

    /// `cd .` (cwd unchanged in basename) doesn't churn a redundant
    /// TabTitleChanged. Guards against per-prompt event spam from a
    /// shell that re-emits the same OSC 7 every prompt.
    #[test]
    fn set_tab_cwd_skips_title_event_when_basename_unchanged() {
        let ws = Workspace::new();
        let pid = ws.create_project("p", "").unwrap().id;
        let tid = ws.open_tab(pid, "/tmp", "").unwrap().id;
        let mut rx = ws.subscribe();
        ws.set_tab_cwd(tid, "/tmp").unwrap();
        // Cwd event fires (same string but the model writes through);
        // title event suppressed since basename didn't change.
        assert!(matches!(
            rx.try_recv(),
            Ok(WorkspaceEvent::TabCwdChanged { .. })
        ));
        assert!(rx.try_recv().is_err());
    }

    /// CLI / IPC `tab.open` callers can pass an explicit placeholder
    /// title (`"roostctl"`, `"Tab 1"`, …). open_tab leaves
    /// user_titled=false on those (the supplied title is treated as
    /// a placeholder per the open_tab comment). The model fix
    /// overwrites the placeholder on the first cwd change.
    /// Guards: a future refactor that flips open_tab to
    /// `user_titled = !title.is_empty()` silently inverts the model
    /// invariant — this test catches it.
    #[test]
    fn set_tab_cwd_overwrites_placeholder_title() {
        let ws = Workspace::new();
        let pid = ws.create_project("p", "").unwrap().id;
        let tid = ws.open_tab(pid, "/tmp", "roostctl").unwrap().id;
        assert_eq!(ws.tab(tid).unwrap().title, "roostctl");
        assert!(!ws.tab(tid).unwrap().user_titled);
        ws.set_tab_cwd(tid, "/usr").unwrap();
        assert_eq!(ws.tab(tid).unwrap().title, "usr");
    }

    /// Cross-platform parity: `derive_title("/")` returns `"/"`,
    /// matching the Swift twin's `(cwd as NSString).lastPathComponent`
    /// for the root case and what shell integration's `__roost_title`
    /// would emit. Without this special-case Path::file_name() returns
    /// None and the fallback `"shell"` diverges from the Mac UI on
    /// `cd /` — the model fix now exercises this routinely.
    #[test]
    fn derive_title_root_returns_slash() {
        assert_eq!(derive_title("/"), "/");
        assert_eq!(derive_title(""), "shell");
        assert_eq!(derive_title("/tmp"), "tmp");
        assert_eq!(derive_title("/usr/local"), "local");
    }

    // ------------------------------------------------------------------
    // Agent state model (plan 002)
    // ------------------------------------------------------------------

    /// A one-tab workspace with the window reported **unfocused**, so
    /// these tests exercise the agent axis without policy §3.5's focus
    /// suppression in the way. `attention_policy_*` covers the matrix.
    fn agent_ws() -> (Workspace, i64) {
        let ws = Workspace::new();
        let pid = ws.create_project("p", "").unwrap().id;
        let tid = ws.open_tab(pid, "/", "").unwrap().id;
        ws.set_window_focused(false);
        (ws, tid)
    }

    fn report(
        tab_id: i64,
        source: &str,
        session: &str,
        action: OwnershipAction,
        lifecycle: Option<AgentLifecycle>,
    ) -> TabAgentReportParams {
        TabAgentReportParams {
            session_id: session.to_string(),
            ..TabAgentReportParams::sessionless(tab_id, source, action, lifecycle)
        }
    }

    /// A workspace whose one tab is already claimed by `claude`/`s1`.
    fn owned_ws(lifecycle: AgentLifecycle) -> (Workspace, i64) {
        let (ws, tid) = agent_ws();
        ws.agent_report(&report(
            tid,
            "claude",
            "s1",
            OwnershipAction::Claim,
            Some(lifecycle),
        ))
        .unwrap();
        (ws, tid)
    }

    /// Replaces `set_tab_state_from_osc_respects_hook_active`: the gate
    /// it pinned is gone. OSC 133 now writes the shell axis
    /// unconditionally, and derivation — not a suppression rule —
    /// decides which axis the tab shows.
    #[test]
    fn shell_marks_write_the_shell_axis_under_agent_ownership() {
        let (ws, tid) = owned_ws(AgentLifecycle::Waiting);

        // The mark lands on the shell axis even while an agent owns the
        // tab; the agent's lifecycle still wins the derived state.
        ws.apply_shell_mark(tid, "C").unwrap();
        let tab = ws.tab(tid).unwrap();
        assert_eq!(tab.shell_state, ShellState::ForegroundProcess);
        assert_eq!(tab.agent_lifecycle, AgentLifecycle::Waiting);
        assert_eq!(tab.state, TabState::NeedsInput);
    }

    #[test]
    fn shell_marks_drive_the_state_without_an_owner() {
        let (ws, tid) = agent_ws();
        ws.apply_shell_mark(tid, "C").unwrap();
        assert_eq!(ws.tab(tid).unwrap().state, TabState::Running);
        ws.apply_shell_mark(tid, "D;0").unwrap();
        assert_eq!(ws.tab(tid).unwrap().state, TabState::None);
    }

    /// The `D`/`A` failsafe end to end: a killed agent's ownership
    /// survives as a label but stops driving derivation, and raw OSC
    /// re-opens.
    #[test]
    fn prompt_mark_reopens_raw_osc_for_a_dead_agent() {
        let (ws, tid) = owned_ws(AgentLifecycle::Working);
        assert!(!ws
            .raise_attention(tid, "Wrapper", "noise", AttentionSource::RawOsc)
            .unwrap());
        assert!(!ws.tab(tid).unwrap().has_notification);

        ws.apply_shell_mark(tid, "D;0").unwrap();
        let tab = ws.tab(tid).unwrap();
        assert_eq!(tab.agent_lifecycle, AgentLifecycle::Inactive);
        assert!(tab.hook_active, "ownership survives as a label");
        assert_eq!(tab.state, TabState::None, "falls through to shell");
        assert!(ws
            .raise_attention(tid, "Build", "done", AttentionSource::RawOsc)
            .unwrap());
        assert!(ws.tab(tid).unwrap().has_notification);
    }

    /// Session scoping is enforced in the workspace, atomically with
    /// the mutation (plan §3.3). A report from a different session is
    /// dropped whole — lifecycle *and* attention.
    #[test]
    fn report_from_a_foreign_session_is_dropped() {
        let (ws, tid) = owned_ws(AgentLifecycle::Working);

        let mut stale = report(
            tid,
            "claude",
            "s2",
            OwnershipAction::Preserve,
            Some(AgentLifecycle::Finished),
        );
        stale.attention = AttentionOp::Set;
        stale.title = "Claude Code".into();
        stale.body = "Turn complete".into();
        let (accepted, tab) = ws.agent_report(&stale).unwrap();
        assert!(!accepted);
        assert_eq!(tab.agent_lifecycle, AgentLifecycle::Working);
        assert!(!tab.has_notification, "a dropped report fires nothing");

        // Same session id, different source: still a mismatch.
        let (accepted, _) = ws
            .agent_report(&report(
                tid,
                "codex",
                "s1",
                OwnershipAction::Preserve,
                Some(AgentLifecycle::Finished),
            ))
            .unwrap();
        assert!(!accepted);
    }

    #[test]
    fn claim_supersedes_a_live_owner_and_release_is_scoped() {
        let (ws, tid) = owned_ws(AgentLifecycle::Working);

        let (accepted, tab) = ws
            .agent_report(&report(
                tid,
                "codex",
                "s9",
                OwnershipAction::Claim,
                Some(AgentLifecycle::Waiting),
            ))
            .unwrap();
        assert!(accepted, "claim is the supersede path");
        assert_eq!(tab.ownership.unwrap().source, "codex");

        // The displaced owner can no longer release.
        let (accepted, tab) = ws
            .agent_report(&report(tid, "claude", "s1", OwnershipAction::Release, None))
            .unwrap();
        assert!(!accepted);
        assert!(tab.hook_active);

        let (accepted, tab) = ws
            .agent_report(&report(tid, "codex", "s9", OwnershipAction::Release, None))
            .unwrap();
        assert!(accepted);
        assert!(!tab.hook_active);
        assert_eq!(tab.agent_lifecycle, AgentLifecycle::Inactive);
    }

    /// Attention effects are the workspace's to apply: `set` raises the
    /// pending flag and fires, `clear` drops it.
    #[test]
    fn attention_set_and_clear_drive_the_notification_flag() {
        let (ws, tid) = agent_ws();
        let mut claim = report(tid, "claude", "s1", OwnershipAction::Claim, None);
        claim.attention = AttentionOp::Set;
        claim.title = "Claude Code".into();
        claim.body = "Needs your permission".into();
        let (_, tab) = ws.agent_report(&claim).unwrap();
        assert!(tab.has_notification);

        let mut clear = report(tid, "claude", "s1", OwnershipAction::Preserve, None);
        clear.attention = AttentionOp::Clear;
        let (_, tab) = ws.agent_report(&clear).unwrap();
        assert!(!tab.has_notification);
    }

    // ------------------------------------------------------------------
    // Attention policy B (plan §3.5) — one transaction, one predicate
    // ------------------------------------------------------------------

    /// A report that sets attention on the focused, active tab. Returns
    /// the tab plus every event the report emitted.
    fn attention_report(tab_id: i64) -> TabAgentReportParams {
        let mut r = report(tab_id, "claude", "s1", OwnershipAction::Claim, None);
        r.attention = AttentionOp::Set;
        r.title = "Claude Code".into();
        r.body = "Turn complete".into();
        r
    }

    fn drain(rx: &mut broadcast::Receiver<WorkspaceEvent>) -> Vec<WorkspaceEvent> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    fn has_attention_events(events: &[WorkspaceEvent]) -> bool {
        events.iter().any(|e| {
            matches!(
                e,
                WorkspaceEvent::NotificationFired { .. }
                    | WorkspaceEvent::TabNotification {
                        has_pending: true,
                        ..
                    }
            )
        })
    }

    /// The tab you are looking at is the tab you have seen: a structured
    /// notification for it is dropped whole — no pending bit, no events.
    /// Emitting nothing is what makes "switch away afterwards and the
    /// badge does not appear" true by construction.
    #[test]
    fn attention_policy_drops_a_notification_for_the_focused_active_tab() {
        let (ws, tid) = agent_ws();
        ws.set_window_focused(true);
        let mut rx = ws.subscribe();

        let (accepted, tab) = ws.agent_report(&attention_report(tid)).unwrap();
        assert!(accepted, "the report itself still applies");
        assert!(!tab.has_notification);
        assert!(!has_attention_events(&drain(&mut rx)));

        // Switching away must not resurrect it.
        let other = ws
            .open_tab(ws.tab(tid).unwrap().project_id, "/", "")
            .unwrap()
            .id;
        assert_ne!(other, tid);
        assert!(!ws.tab(tid).unwrap().has_notification);
    }

    #[test]
    fn attention_policy_delivers_when_the_window_is_unfocused() {
        let (ws, tid) = agent_ws();
        ws.set_window_focused(false);
        let mut rx = ws.subscribe();

        let (_, tab) = ws.agent_report(&attention_report(tid)).unwrap();
        assert!(tab.has_notification);
        assert!(has_attention_events(&drain(&mut rx)));
    }

    #[test]
    fn attention_policy_delivers_when_another_tab_is_active() {
        let (ws, tid) = agent_ws();
        let pid = ws.tab(tid).unwrap().project_id;
        let other = ws.open_tab(pid, "/", "").unwrap().id;
        ws.focus_tab(other).unwrap();
        ws.set_window_focused(true);
        let mut rx = ws.subscribe();

        let (_, tab) = ws.agent_report(&attention_report(tid)).unwrap();
        assert!(tab.has_notification);
        assert!(has_attention_events(&drain(&mut rx)));
    }

    /// Structured attention is NEVER gated on agent ownership (plan
    /// §3.4) — `notification.create` / `roostctl notify` must get
    /// through even mid-turn, since that is how the agent itself speaks.
    #[test]
    fn structured_attention_is_never_gated_by_a_live_agent() {
        let (ws, tid) = owned_ws(AgentLifecycle::Working);

        assert!(ws
            .raise_attention(tid, "Roost", "explicit", AttentionSource::Structured)
            .unwrap());
        assert!(ws.tab(tid).unwrap().has_notification);

        // …and the same is true through the report path.
        ws.set_tab_has_notification(tid, false).unwrap();
        let mut r = attention_report(tid);
        r.ownership_action = OwnershipAction::Preserve;
        let (accepted, tab) = ws.agent_report(&r).unwrap();
        assert!(accepted);
        assert!(tab.has_notification);
    }

    /// Raw OSC is the one thing the agent gate drops, and the `D` mark
    /// failsafe re-opens it (plan §3.4).
    #[test]
    fn raw_osc_attention_is_gated_by_a_live_agent_until_a_prompt_mark() {
        let (ws, tid) = owned_ws(AgentLifecycle::Working);

        assert!(!ws
            .raise_attention(tid, "Wrapper", "noise", AttentionSource::RawOsc)
            .unwrap());
        assert!(!ws.tab(tid).unwrap().has_notification);

        ws.apply_shell_mark(tid, "D;0").unwrap();
        assert!(ws
            .raise_attention(tid, "Build", "done", AttentionSource::RawOsc)
            .unwrap());
        assert!(ws.tab(tid).unwrap().has_notification);
    }

    #[test]
    fn raise_attention_on_a_missing_tab_is_an_error() {
        let ws = Workspace::new();
        assert!(matches!(
            ws.raise_attention(999, "t", "b", AttentionSource::Structured),
            Err(WorkspaceError::TabNotFound(999))
        ));
    }

    /// Focus defaults to *focused* before any UI reports it, so a
    /// headless / IPC-only workspace routes exactly as a real window
    /// would. The active tab is therefore suppressed, and — the half
    /// that keeps the default safe — an inactive tab still delivers.
    #[test]
    fn window_focus_defaults_to_focused() {
        let ws = Workspace::new();
        let pid = ws.create_project("p", "").unwrap().id;
        let active = ws.open_tab(pid, "/", "a").unwrap().id;
        let background = ws.open_tab(pid, "/", "b").unwrap().id;
        ws.focus_tab(active).unwrap();

        assert!(!ws
            .raise_attention(active, "t", "b", AttentionSource::Structured)
            .unwrap());
        assert!(ws
            .raise_attention(background, "t", "b", AttentionSource::Structured)
            .unwrap());
        assert!(ws.tab(background).unwrap().has_notification);
    }

    /// Plan §3.7's transition table. `running`/`needs_input`/`idle`
    /// produce their pre-change `tab.state`; `none` is the one genuine
    /// behavior change — it releases, so the shell axis shows through.
    #[test]
    fn legacy_set_state_follows_the_transition_table() {
        let (ws, tid) = agent_ws();
        for (legacy, lifecycle) in [
            (TabState::Running, AgentLifecycle::Working),
            (TabState::NeedsInput, AgentLifecycle::Waiting),
            (TabState::Idle, AgentLifecycle::Finished),
        ] {
            ws.set_tab_state(tid, legacy).unwrap();
            let tab = ws.tab(tid).unwrap();
            assert_eq!(tab.state, legacy, "legacy projection must not move");
            assert_eq!(tab.agent_lifecycle, lifecycle);
            assert_eq!(tab.ownership.as_ref().unwrap().source, SOURCE_MANUAL);
            assert_eq!(tab.ownership.as_ref().unwrap().session_id, "");
        }

        // `none` releases; with a live foreground process the shell axis
        // now shows `running` rather than `none`.
        ws.apply_shell_mark(tid, "C").unwrap();
        ws.set_tab_state(tid, TabState::None).unwrap();
        let tab = ws.tab(tid).unwrap();
        assert!(!tab.hook_active, "set-state none releases ownership");
        assert_eq!(tab.ownership, None);
        assert_eq!(tab.agent_lifecycle, AgentLifecycle::Inactive);
        assert_eq!(tab.state, TabState::Running, "shell-derived");
    }

    /// A manual override takes the tab from a live agent — the user has
    /// the wheel — and `none` releases even though the release itself is
    /// scoped to `manual`.
    #[test]
    fn manual_override_supersedes_a_live_agent() {
        let (ws, tid) = owned_ws(AgentLifecycle::Working);

        ws.set_tab_state(tid, TabState::NeedsInput).unwrap();
        assert_eq!(
            ws.tab(tid).unwrap().ownership.unwrap().source,
            SOURCE_MANUAL
        );

        // Claude's next in-session event is now out of scope.
        let (accepted, _) = ws
            .agent_report(&report(
                tid,
                "claude",
                "s1",
                OwnershipAction::Preserve,
                Some(AgentLifecycle::Finished),
            ))
            .unwrap();
        assert!(!accepted);

        // And `none` still releases, despite `claude` having held it.
        ws.set_tab_state(tid, TabState::None).unwrap();
        assert_eq!(ws.tab(tid).unwrap().ownership, None);
    }

    #[test]
    fn set_hook_active_claims_and_releases_as_legacy() {
        let (ws, tid) = agent_ws();
        ws.set_tab_hook_active(tid, true).unwrap();
        let tab = ws.tab(tid).unwrap();
        assert!(tab.hook_active);
        assert_eq!(tab.ownership.unwrap().source, SOURCE_LEGACY);
        assert_eq!(
            tab.state,
            TabState::None,
            "ownership alone doesn't move the legacy state"
        );

        ws.set_tab_hook_active(tid, false).unwrap();
        assert!(!ws.tab(tid).unwrap().hook_active);
    }

    #[test]
    fn pty_replacement_clears_ownership() {
        let (ws, tid) = owned_ws(AgentLifecycle::Working);
        ws.apply_shell_mark(tid, "C").unwrap();

        ws.pty_replaced(tid).unwrap();
        let tab = ws.tab(tid).unwrap();
        assert_eq!(tab.ownership, None);
        assert_eq!(tab.agent_lifecycle, AgentLifecycle::Inactive);
        assert_eq!(tab.shell_state, ShellState::Unknown);
        assert_eq!(tab.state, TabState::None);
    }

    /// The derived slices ride along with the full record, so the two
    /// UIs and any external subscriber see one consistent story.
    #[test]
    fn accepted_report_emits_state_hook_and_agent_events() {
        let (ws, tid) = agent_ws();
        let mut rx = ws.subscribe();
        ws.agent_report(&report(
            tid,
            "claude",
            "s1",
            OwnershipAction::Claim,
            Some(AgentLifecycle::Waiting),
        ))
        .unwrap();

        assert!(matches!(
            rx.try_recv(),
            Ok(WorkspaceEvent::TabStateChanged {
                state: TabState::NeedsInput,
                ..
            })
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(WorkspaceEvent::HookActiveChanged { active: true, .. })
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(WorkspaceEvent::AgentChanged { .. })
        ));

        // A report that changes nothing emits nothing — repeated prompt
        // marks are the common case and must not churn the UI.
        ws.apply_shell_mark(tid, "Z").unwrap();
        assert!(rx.try_recv().is_err());
    }

    /// The converse of a dropped report (which succeeds with
    /// `accepted: false`): a tab that doesn't exist is an error.
    #[test]
    fn agent_report_on_a_missing_tab_is_an_error() {
        let ws = Workspace::new();
        assert!(matches!(
            ws.agent_report(&report(999, "claude", "s1", OwnershipAction::Claim, None)),
            Err(WorkspaceError::TabNotFound(999))
        ));
    }

    #[test]
    fn position_is_max_plus_one_after_delete() {
        let ws = Workspace::new();
        let a = ws.create_project("a", "").unwrap();
        let b = ws.create_project("b", "").unwrap();
        let c = ws.create_project("c", "").unwrap();
        assert_eq!((a.position, b.position, c.position), (0, 1, 2));

        // Delete the middle project, then create a new one. The old
        // `len()` rule would reuse position 2 (colliding with c); the
        // fix must hand out max(0, 2) + 1 = 3.
        ws.delete_project(b.id).unwrap();
        let d = ws.create_project("d", "").unwrap();
        assert_eq!(d.position, 3, "new project position collided after delete");

        // Same invariant for tabs within a project.
        let t0 = ws.open_tab(a.id, "/", "t0").unwrap();
        let t1 = ws.open_tab(a.id, "/", "t1").unwrap();
        assert_eq!((t0.position, t1.position), (0, 1));
        ws.close_tab(t0.id).unwrap();
        let t2 = ws.open_tab(a.id, "/", "t2").unwrap();
        assert_eq!(t2.position, 2, "new tab position collided after close");
    }

    #[test]
    fn reorder_tabs_partial_keeps_unlisted() {
        let ws = Workspace::new();
        let pid = ws.create_project("p", "").unwrap().id;
        let a = ws.open_tab(pid, "/", "a").unwrap().id;
        let b = ws.open_tab(pid, "/", "b").unwrap().id;
        let c = ws.open_tab(pid, "/", "c").unwrap().id;
        // Reorder only [c, a] — b should land last.
        ws.reorder_tabs(pid, &[c, a]).unwrap();
        let projects = ws.snapshot();
        let tabs: Vec<i64> = projects[0].tabs.iter().map(|t| t.id).collect();
        assert_eq!(tabs, vec![c, a, b]);
    }

    #[test]
    fn persist_drops_stale_out_of_order_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let ws = Workspace::open(path.clone());

        // A newer commit (seq 2) lands first, then a slower earlier
        // commit (seq 1) races in. The stale write must be dropped so
        // the newest snapshot stays on disk (#80).
        ws.persist(
            2,
            SnapshotFile {
                next_id: 99,
                ..Default::default()
            },
            false,
        );
        ws.persist(
            1,
            SnapshotFile {
                next_id: 5,
                ..Default::default()
            },
            false,
        );
        assert_eq!(
            read_state(&path).unwrap().unwrap().next_id,
            99,
            "stale snapshot overwrote the newer one"
        );

        // A genuinely newer commit (seq 3) still applies.
        ws.persist(
            3,
            SnapshotFile {
                next_id: 200,
                ..Default::default()
            },
            false,
        );
        assert_eq!(read_state(&path).unwrap().unwrap().next_id, 200);
    }

    #[test]
    fn persist_restore_round_trips_tab_layout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let pid = {
            let ws = Workspace::open(path.clone());
            let pid = ws.create_project("p", "/proj").unwrap().id;
            let _a = ws.open_tab(pid, "/a", "atab").unwrap().id;
            let b = ws.open_tab(pid, "/b", "btab").unwrap().id;
            let _c = ws.open_tab(pid, "/c", "ctab").unwrap().id;
            // Select the middle tab so restore picks it by position.
            ws.focus_tab(b).unwrap();
            pid
        };

        let ws2 = Workspace::open(path);
        // The reloaded workspace exposes the layout as restore
        // descriptors, NOT as live tabs.
        assert!(
            ws2.snapshot().iter().all(|p| p.tabs.is_empty()),
            "restored tabs must be descriptors, not live tabs"
        );
        let restore = ws2.take_restore_layout().expect("layout present");
        assert_eq!(restore.active_project_id, pid);
        assert_eq!(restore.active_tab_position, 1, "tab 'b' is at position 1");
        let rp = restore
            .projects
            .iter()
            .find(|p| p.project_id == pid)
            .expect("project in layout");
        assert_eq!(
            rp.tabs.iter().map(|t| t.cwd.as_str()).collect::<Vec<_>>(),
            vec!["/a", "/b", "/c"]
        );
        assert_eq!(rp.tabs[1].title, "btab");
        // `take_restore_layout` is one-shot.
        assert!(ws2.take_restore_layout().is_none());
    }

    #[test]
    fn sidebar_collapsed_persists_across_reopen() {
        // GTK parity with the Mac UI's RoostSidebarVisible: the user's
        // hide/show choice survives quit + relaunch. Backs the locally-run
        // e2e `test_sidebar_collapsed_state_survives_relaunch`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        {
            let ws = Workspace::open(path.clone());
            assert!(!ws.sidebar_collapsed(), "defaults to expanded");
            ws.set_sidebar_collapsed(true);
            assert!(ws.sidebar_collapsed());
        }
        // Reopen: the collapsed choice is restored from disk.
        let ws2 = Workspace::open(path.clone());
        assert!(
            ws2.sidebar_collapsed(),
            "collapsed state must survive reopen"
        );
        // And toggling back to expanded persists too.
        ws2.set_sidebar_collapsed(false);
        drop(ws2);
        let ws3 = Workspace::open(path);
        assert!(
            !ws3.sidebar_collapsed(),
            "expanded state must survive reopen"
        );
    }

    #[test]
    fn active_tab_position_is_dense_index_not_raw_position() {
        // After a mid-project close, positions go sparse (0,1,2 → 1,2).
        // The persisted active_tab_position must be the DENSE index
        // among the surviving tabs (what the UI selects on restore),
        // not the raw `position` field. #95 review.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        {
            let ws = Workspace::open(path.clone());
            let pid = ws.create_project("p", "/").unwrap().id;
            let a = ws.open_tab(pid, "/a", "a").unwrap().id; // position 0
            let _b = ws.open_tab(pid, "/b", "b").unwrap().id; // position 1
            let c = ws.open_tab(pid, "/c", "c").unwrap().id; // position 2
            ws.close_tab(a).unwrap(); // removes position 0 → surviving positions 1,2
            ws.focus_tab(c).unwrap(); // active = c (raw position 2, dense index 1)
        }
        let ws2 = Workspace::open(path);
        let restore = ws2.take_restore_layout().unwrap();
        assert_eq!(
            restore.active_tab_position, 1,
            "active tab is the 2nd surviving tab → dense index 1, not raw position 2"
        );
        // And the surviving tabs are /b, /c in order.
        assert_eq!(
            restore.projects[0]
                .tabs
                .iter()
                .map(|t| t.cwd.as_str())
                .collect::<Vec<_>>(),
            vec!["/b", "/c"]
        );
    }

    /// Issue #196 follow-up: `user_titled` is persisted across
    /// relaunch so a manually-renamed tab keeps its rename — and so
    /// the model's `set_tab_cwd` re-derivation (also #196) doesn't
    /// silently clobber it on the first post-relaunch `cd`.
    #[test]
    fn user_titled_persists_across_relaunch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        {
            let ws = Workspace::open(path.clone());
            let pid = ws.create_project("p", "/").unwrap().id;
            let manual = ws.open_tab(pid, "/tmp", "").unwrap().id;
            let placeholder = ws.open_tab(pid, "/tmp", "roostctl").unwrap().id;
            ws.set_tab_title(manual, "docs").unwrap();
            assert!(ws.tab(manual).unwrap().user_titled);
            assert!(!ws.tab(placeholder).unwrap().user_titled);
        }
        let ws2 = Workspace::open(path);
        let restore = ws2.take_restore_layout().unwrap();
        let tabs = &restore.projects[0].tabs;
        // Two tabs persisted; restore order matches save order.
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].title, "docs");
        assert!(tabs[0].user_titled, "manual rename keeps user_titled");
        assert_eq!(tabs[1].title, "roostctl");
        assert!(!tabs[1].user_titled, "placeholder title is not user_titled");
    }

    #[test]
    fn restore_layout_reflects_persisted_tab_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        {
            let ws = Workspace::open(path.clone());
            let pid = ws.create_project("p", "/").unwrap().id;
            let a = ws.open_tab(pid, "/a", "a").unwrap().id;
            let b = ws.open_tab(pid, "/b", "b").unwrap().id;
            let c = ws.open_tab(pid, "/c", "c").unwrap().id;
            // Reorder to c, a, b — restore must reflect the new order.
            ws.reorder_tabs(pid, &[c, a, b]).unwrap();
        }
        let ws2 = Workspace::open(path);
        let restore = ws2.take_restore_layout().unwrap();
        assert_eq!(
            restore.projects[0]
                .tabs
                .iter()
                .map(|t| t.cwd.as_str())
                .collect::<Vec<_>>(),
            vec!["/c", "/a", "/b"]
        );
    }

    #[test]
    fn cwd_changes_write_through() {
        // No throttle: every `set_tab_cwd` writes through, so a reopen
        // sees the LATEST cwd (last write wins), not a coalesced
        // earlier one. The two calls below are microseconds apart.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        {
            let ws = Workspace::open(path.clone());
            let pid = ws.create_project("p", "/").unwrap().id;
            let tid = ws.open_tab(pid, "/start", "").unwrap().id;
            ws.set_tab_cwd(tid, "/first").unwrap();
            ws.set_tab_cwd(tid, "/second").unwrap();
        }
        let ws2 = Workspace::open(path);
        let restore = ws2.take_restore_layout().unwrap();
        assert_eq!(
            restore.projects[0].tabs[0].cwd, "/second",
            "the latest cwd must reach disk (write-through, no throttle)"
        );
    }

    #[test]
    fn flush_freezes_further_persistence() {
        // flush() writes the current layout (with fsync) and then
        // freezes: a subsequent mutation must NOT reach disk, so a
        // teardown PTY-exit cascade can't clobber the flushed layout.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        {
            let ws = Workspace::open(path.clone());
            let pid = ws.create_project("p", "/").unwrap().id;
            let tid = ws.open_tab(pid, "/flushed", "").unwrap().id;
            ws.flush();
            // Frozen — this write is a no-op.
            ws.set_tab_cwd(tid, "/after-flush").unwrap();
        }
        let ws2 = Workspace::open(path);
        let restore = ws2.take_restore_layout().unwrap();
        assert_eq!(
            restore.projects[0].tabs[0].cwd, "/flushed",
            "a post-flush mutation must not have reached disk"
        );
    }
}
