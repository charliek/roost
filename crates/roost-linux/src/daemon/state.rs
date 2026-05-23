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
//! * **Pre-restart**: orphan tab purging from the legacy daemon is
//!   gone — under the new model, tabs do not persist across UI
//!   quits (the workspace only persists projects + the next-id
//!   counter; tabs come back empty per the no-restore goal).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

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
}

pub struct Workspace {
    inner: Mutex<Inner>,
    events: broadcast::Sender<WorkspaceEvent>,
    /// Where to write the `state.json` file. `None` means the
    /// in-memory variant (used by tests).
    state_path: Option<PathBuf>,
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
        // Tabs are intentionally NOT restored from the snapshot
        // file (the no-session-restore goal). state.json only
        // carries projects + next_id.
        Self {
            inner: Mutex::new(inner),
            events: tx,
            state_path: Some(state_path),
        }
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
            let mut active_changed = false;
            if inner.active_project_id == 0 {
                inner.active_project_id = id;
                active_changed = true;
            }
            let (apid, atid) = (inner.active_project_id, inner.active_tab_id);
            drop(inner);
            if active_changed {
                let _ = self.events.send(WorkspaceEvent::ActiveChanged {
                    project_id: apid,
                    tab_id: atid,
                });
            }
            return id;
        }
        let id = inner.alloc_id();
        let position = inner.projects.len() as i32;
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
        let snapshot = inner.snapshot_for_persist();
        let project = Project {
            id,
            name: "Default".into(),
            cwd: cwd.to_string(),
            position,
            created_at: now,
            tabs: vec![],
        };
        drop(inner);
        self.persist_async(snapshot);
        let _ = self.events.send(WorkspaceEvent::ProjectCreated(project));
        let _ = self.events.send(WorkspaceEvent::ActiveChanged {
            project_id: id,
            tab_id: 0,
        });
        id
    }

    pub fn create_project(&self, name: &str, cwd: &str) -> Result<Project, WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let id = inner.alloc_id();
        let position = inner.projects.len() as i32;
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
        let snapshot = inner.snapshot_for_persist();
        drop(inner);

        let project = Project {
            id: row.id,
            name: row.name,
            cwd: row.cwd,
            position: row.position,
            created_at: row.created_at,
            tabs: vec![],
        };
        self.persist_async(snapshot);
        let _ = self
            .events
            .send(WorkspaceEvent::ProjectCreated(project.clone()));
        Ok(project)
    }

    pub fn rename_project(&self, project_id: i64, name: &str) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let row = inner
            .projects
            .get_mut(&project_id)
            .ok_or(WorkspaceError::ProjectNotFound(project_id))?;
        row.name = name.to_string();
        let snapshot = inner.snapshot_for_persist();
        drop(inner);

        self.persist_async(snapshot);
        let _ = self.events.send(WorkspaceEvent::ProjectRenamed {
            project_id,
            name: name.to_string(),
        });
        Ok(())
    }

    /// Delete a project. Cascades to its tabs (per-tab
    /// `TabClosed` events emitted first, then `ProjectDeleted`,
    /// then `ActiveChanged` if the selection moved). PTY cleanup
    /// is the caller's responsibility — the workspace doesn't own
    /// a `PtySupervisor` reference.
    pub fn delete_project(&self, project_id: i64) -> Result<Vec<i64>, WorkspaceError> {
        let (deleted_tab_ids, active_now) = {
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
            let snapshot = inner.snapshot_for_persist();
            drop(inner);
            self.persist_async(snapshot);
            (tab_ids, active)
        };

        for tid in &deleted_tab_ids {
            let _ = self.events.send(WorkspaceEvent::TabClosed { tab_id: *tid });
        }
        let _ = self
            .events
            .send(WorkspaceEvent::ProjectDeleted { project_id });
        if let Some((pid, tid)) = active_now {
            let _ = self.events.send(WorkspaceEvent::ActiveChanged {
                project_id: pid,
                tab_id: tid,
            });
        }
        Ok(deleted_tab_ids)
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
        let position = inner
            .tabs
            .values()
            .filter(|t| t.project_id == project_id)
            .count() as i32;
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
        let snapshot = inner.snapshot_for_persist();
        drop(inner);

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
        self.persist_async(snapshot);
        let _ = self.events.send(WorkspaceEvent::TabOpened(tab.clone()));
        let _ = self.events.send(WorkspaceEvent::ActiveChanged {
            project_id,
            tab_id: id,
        });
        Ok(tab)
    }

    pub fn close_tab(&self, tab_id: i64) -> Result<(), WorkspaceError> {
        let active_now = {
            let mut inner = self.inner.lock().unwrap();
            let row = inner
                .tabs
                .remove(&tab_id)
                .ok_or(WorkspaceError::TabNotFound(tab_id))?;
            // If we closed the active tab, fall back to any tab in
            // the same project, otherwise any tab anywhere.
            let mut changed = false;
            if inner.active_tab_id == tab_id {
                let next = inner
                    .tabs
                    .values()
                    .find(|t| t.project_id == row.project_id)
                    .or_else(|| inner.tabs.values().next())
                    .map(|t| (t.project_id, t.id))
                    .unwrap_or((row.project_id, 0));
                inner.active_project_id = next.0;
                inner.active_tab_id = next.1;
                changed = true;
            }
            let active = if changed {
                Some((inner.active_project_id, inner.active_tab_id))
            } else {
                None
            };
            let snapshot = inner.snapshot_for_persist();
            drop(inner);
            self.persist_async(snapshot);
            active
        };

        let _ = self.events.send(WorkspaceEvent::TabClosed { tab_id });
        if let Some((pid, tid)) = active_now {
            let _ = self.events.send(WorkspaceEvent::ActiveChanged {
                project_id: pid,
                tab_id: tid,
            });
        }
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
        drop(inner);
        let _ = self.events.send(WorkspaceEvent::TabTitleChanged {
            tab_id,
            title: title.to_string(),
        });
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
        drop(inner);
        let _ = self.events.send(WorkspaceEvent::TabTitleChanged {
            tab_id,
            title: title.to_string(),
        });
        Ok(())
    }

    pub fn set_tab_cwd(&self, tab_id: i64, cwd: &str) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let row = inner
            .tabs
            .get_mut(&tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;
        row.cwd = cwd.to_string();
        drop(inner);
        let _ = self.events.send(WorkspaceEvent::TabCwdChanged {
            tab_id,
            cwd: cwd.to_string(),
        });
        Ok(())
    }

    pub fn set_tab_state(&self, tab_id: i64, state: TabState) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let row = inner
            .tabs
            .get_mut(&tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;
        row.state = state;
        drop(inner);
        let _ = self
            .events
            .send(WorkspaceEvent::TabStateChanged { tab_id, state });
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
        drop(inner);
        let _ = self.events.send(WorkspaceEvent::TabNotification {
            tab_id,
            has_pending,
        });
        Ok(())
    }

    pub fn set_tab_hook_active(&self, tab_id: i64, active: bool) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let row = inner
            .tabs
            .get_mut(&tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;
        row.hook_active = active;
        drop(inner);
        let _ = self
            .events
            .send(WorkspaceEvent::HookActiveChanged { tab_id, active });
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
        drop(inner);
        let _ = self.events.send(WorkspaceEvent::ActiveChanged {
            project_id: row.project_id,
            tab_id: row.id,
        });
        Ok(prev)
    }

    pub fn fire_notification(
        &self,
        tab_id: i64,
        title: &str,
        body: &str,
    ) -> Result<(), WorkspaceError> {
        // Drop a 'lookup' error if the tab is gone — useful for
        // hook tools that race tab close.
        let exists = self.inner.lock().unwrap().tabs.contains_key(&tab_id);
        if !exists {
            return Err(WorkspaceError::TabNotFound(tab_id));
        }
        let _ = self.events.send(WorkspaceEvent::NotificationFired {
            tab_id,
            title: title.to_string(),
            body: body.to_string(),
        });
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
        let snapshot = inner.snapshot_for_persist();
        drop(inner);
        self.persist_async(snapshot);
        let _ = self.events.send(WorkspaceEvent::TabsReordered {
            project_id,
            tab_ids: final_order,
        });
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
        let snapshot = inner.snapshot_for_persist();
        drop(inner);
        self.persist_async(snapshot);
        let _ = self.events.send(WorkspaceEvent::ProjectsReordered {
            project_ids: final_order,
        });
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

    fn persist_async(&self, snapshot: SnapshotFile) {
        let Some(path) = self.state_path.clone() else {
            return; // in-memory variant; no persistence
        };
        // Persistence runs on the caller's thread. The mutation
        // lock is already released; tokio main-thread doesn't block
        // the IPC accept loop because writes are atomic-rename and
        // small. If it becomes a hot path the future-proof move is
        // to spawn a single dedicated writer task.
        if let Err(err) = persist_state(&path, &snapshot) {
            warn!(?err, "failed to persist state.json");
        }
    }
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

    fn snapshot_for_persist(&self) -> SnapshotFile {
        SnapshotFile {
            next_id: self.next_id,
            projects: self
                .projects
                .values()
                .map(|p| crate::daemon::store_json::ProjectSnapshot {
                    id: p.id,
                    name: p.name.clone(),
                    cwd: p.cwd.clone(),
                    position: p.position,
                    created_at: p.created_at,
                })
                .collect(),
        }
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
}
