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
    state: TabState,
    has_notification: bool,
    user_titled: bool,
    position: i32,
    created_at: i64,
    last_active: i64,
    hook_active: bool,
}

#[derive(Default)]
struct Inner {
    projects: BTreeMap<i64, ProjectRow>,
    tabs: BTreeMap<i64, TabRow>,
    next_id: i64,
    active_project_id: i64,
    active_tab_id: i64,
    /// Monotonic commit counter, bumped each time a persistable
    /// snapshot is taken (under this lock). Tags each snapshot so
    /// `persist()` can drop stale out-of-order writes (#80).
    persist_seq: u64,
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
            state: TabState::None,
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
            hook_active: false,
        };
        inner.tabs.insert(id, row.clone());
        // New tabs steal the active selection.
        inner.active_project_id = project_id;
        inner.active_tab_id = id;

        let tab = Tab {
            id: row.id,
            project_id: row.project_id,
            title: row.title,
            cwd: row.cwd,
            state: row.state,
            has_notification: row.has_notification,
            is_active: true,
            user_titled: row.user_titled,
            position: row.position,
            created_at: row.created_at,
            last_active: row.last_active,
            hook_active: row.hook_active,
        };
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
        row.cwd = cwd.to_string();
        // Shell-driven (OSC 7 fires per `cd`) but write-through: each
        // change lands in the page cache without an fsync, so a `cd`
        // loop is cheap and the latest cwd is always on disk.
        self.commit(
            inner,
            vec![WorkspaceEvent::TabCwdChanged {
                tab_id,
                cwd: cwd.to_string(),
            }],
            Persist::Write,
        );
        Ok(())
    }

    pub fn set_tab_state(&self, tab_id: i64, state: TabState) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let row = inner
            .tabs
            .get_mut(&tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;
        row.state = state;
        // Run-state isn't in the persisted snapshot — emit only.
        self.commit(
            inner,
            vec![WorkspaceEvent::TabStateChanged { tab_id, state }],
            Persist::Skip,
        );
        Ok(())
    }

    /// OSC 133 prompt/command mark → run state. Suppressed while a Claude
    /// hook owns the tab (`hook_active`): the hook's per-turn state wins.
    /// Mirrors `set_tab_title_from_osc`'s `user_titled` gate — NOT
    /// `set_tab_state` (the hook's own `tab.set_state` op must stay ungated).
    pub fn set_tab_state_from_osc(
        &self,
        tab_id: i64,
        state: TabState,
    ) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let row = inner
            .tabs
            .get_mut(&tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;
        if row.hook_active {
            return Ok(());
        }
        row.state = state;
        self.commit(
            inner,
            vec![WorkspaceEvent::TabStateChanged { tab_id, state }],
            Persist::Skip,
        );
        Ok(())
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

    pub fn set_tab_hook_active(&self, tab_id: i64, active: bool) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let row = inner
            .tabs
            .get_mut(&tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;
        row.hook_active = active;
        // Hook-active flag isn't in the persisted snapshot — emit only.
        self.commit(
            inner,
            vec![WorkspaceEvent::HookActiveChanged { tab_id, active }],
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

    pub fn fire_notification(
        &self,
        tab_id: i64,
        title: &str,
        body: &str,
    ) -> Result<(), WorkspaceError> {
        // Drop a 'lookup' error if the tab is gone — useful for
        // hook tools that race tab close. Hold the lock across the
        // existence check + publish so the event can't be reordered
        // past a concurrent close of the same tab.
        let inner = self.inner.lock().unwrap();
        if !inner.tabs.contains_key(&tab_id) {
            return Err(WorkspaceError::TabNotFound(tab_id));
        }
        // Transient notification — nothing persisted, emit only.
        self.commit(
            inner,
            vec![WorkspaceEvent::NotificationFired {
                tab_id,
                title: title.to_string(),
                body: body.to_string(),
            }],
            Persist::Skip,
        );
        Ok(())
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
        let row = inner
            .tabs
            .get(&tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?
            .clone();
        Ok(self.to_wire_tab(&row, &inner))
    }

    fn to_wire_tab(&self, row: &TabRow, inner: &Inner) -> Tab {
        Tab {
            id: row.id,
            project_id: row.project_id,
            title: row.title.clone(),
            cwd: row.cwd.clone(),
            state: row.state,
            has_notification: row.has_notification,
            is_active: inner.active_tab_id == row.id,
            user_titled: row.user_titled,
            position: row.position,
            created_at: row.created_at,
            last_active: row.last_active,
            hook_active: row.hook_active,
        }
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

/// Whether a `commit()` should write `state.json`. `Write` for layout
/// changes (projects/tabs/order/active selection); `Skip` for state
/// that isn't in the persisted snapshot (run-state, notification +
/// hook flags) — those emit an event but never touch disk.
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
    std::path::Path::new(cwd)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "shell".into())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn set_tab_state_from_osc_respects_hook_active() {
        let ws = Workspace::new();
        let pid = ws.create_project("p", "").unwrap().id;
        let tid = ws.open_tab(pid, "/", "").unwrap().id;
        // Hook owns the tab: OSC 133 state is suppressed.
        ws.set_tab_hook_active(tid, true).unwrap();
        ws.set_tab_state_from_osc(tid, TabState::Running).unwrap();
        assert_eq!(ws.tab(tid).unwrap().state, TabState::None);
        // Release the hook: OSC 133 state applies.
        ws.set_tab_hook_active(tid, false).unwrap();
        ws.set_tab_state_from_osc(tid, TabState::Running).unwrap();
        assert_eq!(ws.tab(tid).unwrap().state, TabState::Running);
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
