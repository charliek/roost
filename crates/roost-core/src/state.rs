//! In-memory workspace state.
//!
//! Tracks projects, tabs, the active selection, and an event broadcaster
//! that powers `WatchEvents`. Persistence (SQLite) is not wired in Phase 3
//! — when it lands, the public API of this module stays the same; only
//! the storage backend changes.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::broadcast;

use roost_proto::v1::{
    Event, NotificationEvent, Project, ProjectsReorderedEvent, Tab, TabCwdChangedEvent,
    TabDeletedEvent, TabNotificationEvent, TabState, TabStateChangedEvent, TabTitleChangedEvent,
    TabsReorderedEvent,
};

/// How many events the broadcast channel buffers per subscriber. UI clients
/// that fall this far behind get a `Lagged` and resync via `ListTabs`.
const EVENT_CHANNEL_CAPACITY: usize = 256;

#[derive(Clone, Debug)]
pub struct StoredProject {
    pub id: i64,
    pub name: String,
    pub cwd: String,
    pub position: i32,
    pub created_at: i64,
}

#[derive(Clone, Debug)]
pub struct StoredTab {
    pub id: i64,
    pub project_id: i64,
    pub title: String,
    pub cwd: String,
    pub state: TabState,
    pub has_notification: bool,
    pub user_titled: bool,
    pub position: i32,
    pub created_at: i64,
    pub last_active: i64,
    pub hook_active: bool,
}

pub struct Workspace {
    inner: Mutex<Inner>,
    events: broadcast::Sender<Event>,
}

struct Inner {
    projects: Vec<StoredProject>,
    tabs: Vec<StoredTab>,
    next_project_id: AtomicI64,
    next_tab_id: AtomicI64,
    active_project_id: i64,
    active_tab_id: i64,
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}

impl Workspace {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            inner: Mutex::new(Inner {
                projects: Vec::new(),
                tabs: Vec::new(),
                next_project_id: AtomicI64::new(1),
                next_tab_id: AtomicI64::new(1),
                active_project_id: 0,
                active_tab_id: 0,
            }),
            events: tx,
        }
    }

    /// Subscribe to the event broadcast channel.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn snapshot(&self) -> Vec<Project> {
        let inner = self.inner.lock().unwrap();
        let mut projects: Vec<&StoredProject> = inner.projects.iter().collect();
        projects.sort_by_key(|p| p.position);

        projects
            .into_iter()
            .map(|p| {
                let mut tabs: Vec<&StoredTab> =
                    inner.tabs.iter().filter(|t| t.project_id == p.id).collect();
                tabs.sort_by_key(|t| t.position);
                Project {
                    id: p.id,
                    name: p.name.clone(),
                    cwd: p.cwd.clone(),
                    position: p.position,
                    created_at: p.created_at,
                    tabs: tabs.into_iter().map(|t| to_proto_tab(t, &inner)).collect(),
                }
            })
            .collect()
    }

    pub fn active(&self) -> (i64, i64) {
        let inner = self.inner.lock().unwrap();
        (inner.active_project_id, inner.active_tab_id)
    }

    pub fn ensure_default_project(&self, cwd: &str) -> i64 {
        let mut inner = self.inner.lock().unwrap();
        if let Some(p) = inner.projects.first() {
            return p.id;
        }
        let id = inner.next_project_id.fetch_add(1, Ordering::Relaxed);
        let project = StoredProject {
            id,
            name: "Default".into(),
            cwd: cwd.into(),
            position: 0,
            created_at: now_secs(),
        };
        inner.projects.push(project);
        inner.active_project_id = id;
        id
    }

    pub fn open_tab(
        &self,
        project_id: i64,
        cwd: &str,
        title: &str,
    ) -> Result<StoredTab, WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.projects.iter().any(|p| p.id == project_id) {
            return Err(WorkspaceError::ProjectNotFound(project_id));
        }
        let id = inner.next_tab_id.fetch_add(1, Ordering::Relaxed);
        let position = inner
            .tabs
            .iter()
            .filter(|t| t.project_id == project_id)
            .map(|t| t.position)
            .max()
            .map(|p| p + 1)
            .unwrap_or(0);
        let title = if title.is_empty() {
            derive_title_from_cwd(cwd)
        } else {
            title.to_string()
        };
        let tab = StoredTab {
            id,
            project_id,
            title,
            cwd: cwd.into(),
            state: TabState::None,
            has_notification: false,
            user_titled: false,
            position,
            created_at: now_secs(),
            last_active: now_secs(),
            hook_active: false,
        };
        inner.tabs.push(tab.clone());
        if inner.active_tab_id == 0 {
            inner.active_tab_id = id;
        }
        Ok(tab)
    }

    pub fn close_tab(&self, tab_id: i64) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let pos = inner
            .tabs
            .iter()
            .position(|t| t.id == tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;
        inner.tabs.remove(pos);
        if inner.active_tab_id == tab_id {
            inner.active_tab_id = inner.tabs.first().map(|t| t.id).unwrap_or(0);
        }
        drop(inner);
        let _ = self.events.send(Event {
            kind: Some(roost_proto::v1::event::Kind::TabDeleted(TabDeletedEvent {
                tab_id,
            })),
        });
        Ok(())
    }

    pub fn focus_tab(&self, tab_id: i64) -> Result<(i64, i64), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let (project_id, id) = inner
            .tabs
            .iter()
            .find(|t| t.id == tab_id)
            .map(|t| (t.project_id, t.id))
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;
        let prev = (inner.active_project_id, inner.active_tab_id);
        inner.active_project_id = project_id;
        inner.active_tab_id = id;
        Ok(prev)
    }

    pub fn set_tab_title(
        &self,
        tab_id: i64,
        title: &str,
        user: bool,
    ) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let tab = inner
            .tabs
            .iter_mut()
            .find(|t| t.id == tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;
        // Per the legacy 0002_user_titled migration: OSC writes don't override
        // a manually-renamed tab. `user` distinguishes the two write paths.
        if !user && tab.user_titled {
            return Ok(());
        }
        tab.title = title.to_string();
        if user {
            tab.user_titled = true;
        }
        let title_clone = tab.title.clone();
        drop(inner);
        let _ = self.events.send(Event {
            kind: Some(roost_proto::v1::event::Kind::TabTitle(
                TabTitleChangedEvent {
                    tab_id,
                    title: title_clone,
                },
            )),
        });
        Ok(())
    }

    pub fn set_tab_state(&self, tab_id: i64, state: TabState) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let tab = inner
            .tabs
            .iter_mut()
            .find(|t| t.id == tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;
        tab.state = state;
        drop(inner);
        let _ = self.events.send(Event {
            kind: Some(roost_proto::v1::event::Kind::TabState(
                TabStateChangedEvent {
                    tab_id,
                    state: state as i32,
                },
            )),
        });
        Ok(())
    }

    pub fn set_tab_cwd(&self, tab_id: i64, cwd: &str) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let tab = inner
            .tabs
            .iter_mut()
            .find(|t| t.id == tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;
        tab.cwd = cwd.to_string();
        drop(inner);
        let _ = self.events.send(Event {
            kind: Some(roost_proto::v1::event::Kind::TabCwd(TabCwdChangedEvent {
                tab_id,
                cwd: cwd.to_string(),
            })),
        });
        Ok(())
    }

    pub fn set_tab_notification(
        &self,
        tab_id: i64,
        has_pending: bool,
    ) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let tab = inner
            .tabs
            .iter_mut()
            .find(|t| t.id == tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;
        tab.has_notification = has_pending;
        drop(inner);
        let _ = self.events.send(Event {
            kind: Some(roost_proto::v1::event::Kind::TabNotification(
                TabNotificationEvent {
                    tab_id,
                    has_pending,
                },
            )),
        });
        Ok(())
    }

    pub fn set_hook_active(&self, tab_id: i64, active: bool) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let tab = inner
            .tabs
            .iter_mut()
            .find(|t| t.id == tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;
        tab.hook_active = active;
        Ok(())
    }

    pub fn fire_notification(
        &self,
        tab_id: i64,
        title: &str,
        body: &str,
    ) -> Result<(), WorkspaceError> {
        // Mark the tab as having a pending notification, then broadcast.
        self.set_tab_notification(tab_id, true)?;
        let _ = self.events.send(Event {
            kind: Some(roost_proto::v1::event::Kind::Notification(
                NotificationEvent {
                    tab_id,
                    title: title.into(),
                    body: body.into(),
                },
            )),
        });
        Ok(())
    }

    /// Reports a `tabs_reordered` and a `projects_reordered` event for the
    /// current snapshot. Useful when the UI requests a coarse resync.
    pub fn broadcast_structural_resync(&self) {
        let snapshot = self.snapshot();
        let project_ids: Vec<i64> = snapshot.iter().map(|p| p.id).collect();
        let _ = self.events.send(Event {
            kind: Some(roost_proto::v1::event::Kind::ProjectsReordered(
                ProjectsReorderedEvent { project_ids },
            )),
        });
        for project in &snapshot {
            let tab_ids: Vec<i64> = project.tabs.iter().map(|t| t.id).collect();
            let _ = self.events.send(Event {
                kind: Some(roost_proto::v1::event::Kind::TabsReordered(
                    TabsReorderedEvent {
                        project_id: project.id,
                        tab_ids,
                    },
                )),
            });
        }
    }

    pub fn tab(&self, tab_id: i64) -> Option<StoredTab> {
        let inner = self.inner.lock().unwrap();
        inner.tabs.iter().find(|t| t.id == tab_id).cloned()
    }
}

fn to_proto_tab(t: &StoredTab, inner: &Inner) -> Tab {
    Tab {
        id: t.id,
        project_id: t.project_id,
        title: t.title.clone(),
        cwd: t.cwd.clone(),
        state: t.state as i32,
        has_notification: t.has_notification,
        is_active: inner.active_tab_id == t.id,
        user_titled: t.user_titled,
        position: t.position,
        created_at: t.created_at,
        last_active: t.last_active,
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn derive_title_from_cwd(cwd: &str) -> String {
    std::path::Path::new(cwd)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| cwd.to_string())
}

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("project {0} not found")]
    ProjectNotFound(i64),
    #[error("tab {0} not found")]
    TabNotFound(i64),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_close_tab() {
        let ws = Workspace::new();
        let project = ws.ensure_default_project("/tmp");
        let tab = ws.open_tab(project, "/tmp/work", "").unwrap();
        assert_eq!(ws.snapshot()[0].tabs.len(), 1);
        ws.close_tab(tab.id).unwrap();
        assert!(ws.snapshot()[0].tabs.is_empty());
    }

    #[test]
    fn user_titled_locks_against_osc() {
        let ws = Workspace::new();
        let project = ws.ensure_default_project("/tmp");
        let tab = ws.open_tab(project, "/tmp", "initial").unwrap();
        ws.set_tab_title(tab.id, "manual", true).unwrap();
        ws.set_tab_title(tab.id, "from-osc", false).unwrap();
        let after = ws.tab(tab.id).unwrap();
        assert_eq!(after.title, "manual");
        assert!(after.user_titled);
    }
}
