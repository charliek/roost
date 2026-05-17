//! Workspace state.
//!
//! Persistent fields (project + tab rows) live in SQLite via [`crate::store`].
//! Runtime-only fields — agent state, has_notification flag, hook-active
//! flag, active project/tab selection — live in an in-memory `RuntimeState`
//! and reset on daemon restart. The Go side made the same split: the
//! `core.Workspace` struct held the ephemeral fields while `internal/store`
//! owned the persisted ones.
//!
//! All mutators emit corresponding `Event`s on the broadcast channel that
//! powers `WatchEvents`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use tokio::sync::broadcast;
use tracing::warn;

use roost_proto::v1::{
    ActiveChangedEvent, Event, HookActiveChangedEvent, NotificationEvent, Project,
    ProjectCreatedEvent, ProjectDeletedEvent, ProjectRenamedEvent, ProjectsReorderedEvent, Tab,
    TabCwdChangedEvent, TabDeletedEvent, TabNotificationEvent, TabOpenedEvent, TabState,
    TabStateChangedEvent, TabTitleChangedEvent, TabsReorderedEvent,
};

use crate::store::{Store, StoreError};

/// How many events the broadcast channel buffers per subscriber. Subscribers
/// that fall behind get a `Lagged` and resync via `ListTabs`.
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// One project as exposed by `Workspace::snapshot`-adjacent helpers. Mirrors
/// the persisted columns; the proto `Project` is built from this plus the
/// project's tabs.
#[derive(Clone, Debug)]
pub struct StoredProject {
    pub id: i64,
    pub name: String,
    pub cwd: String,
    pub position: i32,
    pub created_at: i64,
}

/// One tab as exposed by `Workspace::open_tab` / `Workspace::tab`. Combines
/// persisted columns (from SQLite) with runtime-only flags (agent state,
/// pending notification, hook-active suppression).
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

#[derive(Clone, Copy, Default)]
struct RuntimeTab {
    state: TabState,
    has_notification: bool,
    hook_active: bool,
}

#[derive(Default)]
struct RuntimeState {
    tabs: HashMap<i64, RuntimeTab>,
    active_project_id: i64,
    active_tab_id: i64,
}

pub struct Workspace {
    store: Mutex<Store>,
    runtime: Mutex<RuntimeState>,
    events: broadcast::Sender<Event>,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("project {0} not found")]
    ProjectNotFound(i64),
    #[error("tab {0} not found")]
    TabNotFound(i64),
    #[error("store: {0}")]
    Store(StoreError),
}

/// Convert a `StoreError` to a `WorkspaceError` while preserving the precise
/// `ProjectNotFound` / `TabNotFound` variants. Unlike a blanket `From`, this
/// can't accidentally swallow not-found into the catch-all `Store(_)` case
/// when used with `?`.
fn wrap(err: StoreError) -> WorkspaceError {
    match err {
        StoreError::ProjectNotFound(id) => WorkspaceError::ProjectNotFound(id),
        StoreError::TabNotFound(id) => WorkspaceError::TabNotFound(id),
        other => WorkspaceError::Store(other),
    }
}

impl Workspace {
    /// In-memory workspace. The schema is migrated immediately. Use
    /// `Workspace::open` for a file-backed runtime.
    pub fn new() -> Self {
        let store = Store::in_memory().expect("in-memory store should always open");
        Self::with_store(store)
    }

    /// Open a file-backed workspace at `path`, creating + migrating the DB
    /// if needed.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, WorkspaceError> {
        let store = Store::open(path).map_err(wrap)?;
        // Purge orphan tab rows from prior daemon sessions. A Tab
        // implies a live PTY; PTYs don't survive a process restart,
        // so any rows present at boot are stale by definition. Without
        // this, M5's empty-project cascade misfires: the user types
        // `exit` in their last visible tab, the daemon-side
        // list_tabs(project_id) still returns the orphans, the project
        // looks non-empty, and the cascade never deletes the project.
        // Projects are preserved across restarts; only tabs are stale.
        match store.delete_all_tabs() {
            Ok(n) if n > 0 => {
                warn!(removed = n, "purged orphan tab rows at daemon startup");
            }
            Ok(_) => {}
            Err(err) => {
                warn!(?err, "failed to purge orphan tab rows at startup");
            }
        }
        Ok(Self::with_store(store))
    }

    fn with_store(store: Store) -> Self {
        let (tx, _rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            store: Mutex::new(store),
            runtime: Mutex::new(RuntimeState::default()),
            events: tx,
        }
    }

    /// Subscribe to the event broadcast channel.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn snapshot(&self) -> Vec<Project> {
        let store = self.store.lock().unwrap();
        let runtime = self.runtime.lock().unwrap();
        let projects = match store.list_projects() {
            Ok(p) => p,
            Err(err) => {
                warn!(?err, "snapshot: list_projects failed");
                return Vec::new();
            }
        };
        projects
            .into_iter()
            .map(|p| {
                let tabs = match store.list_tabs(p.id) {
                    Ok(rows) => rows,
                    Err(err) => {
                        warn!(project_id = p.id, ?err, "snapshot: list_tabs failed");
                        Vec::new()
                    }
                };
                Project {
                    id: p.id,
                    name: p.name,
                    cwd: p.cwd,
                    position: p.position,
                    created_at: p.created_at,
                    tabs: tabs.into_iter().map(|t| merge_tab(t, &runtime)).collect(),
                }
            })
            .collect()
    }

    pub fn active(&self) -> (i64, i64) {
        let r = self.runtime.lock().unwrap();
        (r.active_project_id, r.active_tab_id)
    }

    pub fn ensure_default_project(&self, cwd: &str) -> i64 {
        let store = self.store.lock().unwrap();
        if let Ok(projects) = store.list_projects() {
            if let Some(p) = projects.first() {
                let mut runtime = self.runtime.lock().unwrap();
                let mut active_changed = false;
                if runtime.active_project_id == 0 {
                    runtime.active_project_id = p.id;
                    active_changed = true;
                }
                let id = p.id;
                drop(runtime);
                if active_changed {
                    self.emit_active_changed();
                }
                return id;
            }
        }
        let project = match store.create_project("Default", cwd) {
            Ok(p) => p,
            Err(err) => {
                warn!(?err, "ensure_default_project: create_project failed");
                return 0;
            }
        };
        let mut runtime = self.runtime.lock().unwrap();
        runtime.active_project_id = project.id;
        let id = project.id;
        drop(runtime);
        self.emit_active_changed();
        id
    }

    /// Create a project. Empty `name` yields a daemon-picked
    /// `"Untitled <n>"` so a UI's "+" button can defer naming until
    /// the user types into the row.
    pub fn create_project(&self, name: &str, cwd: &str) -> Result<StoredProject, WorkspaceError> {
        let store = self.store.lock().unwrap();
        let chosen_name = if name.is_empty() {
            let n = store.list_projects().map_err(wrap)?.len() + 1;
            format!("Untitled {n}")
        } else {
            name.to_string()
        };
        let row = store.create_project(&chosen_name, cwd).map_err(wrap)?;
        drop(store);

        let stored = StoredProject {
            id: row.id,
            name: row.name.clone(),
            cwd: row.cwd.clone(),
            position: row.position,
            created_at: row.created_at,
        };
        let _ = self.events.send(Event {
            kind: Some(roost_proto::v1::event::Kind::ProjectCreated(
                ProjectCreatedEvent {
                    project: Some(Project {
                        id: row.id,
                        name: row.name,
                        cwd: row.cwd,
                        position: row.position,
                        created_at: row.created_at,
                        tabs: vec![],
                    }),
                },
            )),
        });
        Ok(stored)
    }

    pub fn rename_project(&self, project_id: i64, name: &str) -> Result<(), WorkspaceError> {
        let store = self.store.lock().unwrap();
        store.rename_project(project_id, name).map_err(wrap)?;
        drop(store);
        let _ = self.events.send(Event {
            kind: Some(roost_proto::v1::event::Kind::ProjectRenamed(
                ProjectRenamedEvent {
                    project_id,
                    name: name.to_string(),
                },
            )),
        });
        Ok(())
    }

    /// Delete a project and all its tabs. The store's CASCADE drops
    /// tab rows server-side; we mirror that by:
    ///   * collecting the doomed tab ids BEFORE the SQL delete so
    ///     subscribers see one `TabDeletedEvent` per tab,
    ///   * dropping the per-tab runtime entries (state, hook flag),
    ///   * computing a fallback active `(project, tab)` if the
    ///     deletion took out the current selection.
    /// Order of events on the wire: per-tab `TabDeletedEvent`s, then
    /// `ProjectDeletedEvent`, then `ActiveChangedEvent` if the
    /// selection moved.
    pub fn delete_project(&self, project_id: i64) -> Result<(), WorkspaceError> {
        let (deleted_tab_ids, fallback) = {
            let store = self.store.lock().unwrap();
            let projects = store.list_projects().map_err(wrap)?;
            if !projects.iter().any(|p| p.id == project_id) {
                return Err(WorkspaceError::ProjectNotFound(project_id));
            }
            let tab_ids: Vec<i64> = store
                .list_tabs(project_id)
                .map_err(wrap)?
                .into_iter()
                .map(|t| t.id)
                .collect();
            let fallback = projects
                .iter()
                .filter(|p| p.id != project_id)
                .find_map(|p| {
                    store
                        .list_tabs(p.id)
                        .ok()
                        .and_then(|tabs| tabs.into_iter().next().map(|t| (t.project_id, t.id)))
                });
            store.delete_project(project_id).map_err(wrap)?;
            (tab_ids, fallback)
        };

        let mut active_changed = false;
        {
            let mut runtime = self.runtime.lock().unwrap();
            for tid in &deleted_tab_ids {
                runtime.tabs.remove(tid);
            }
            if runtime.active_project_id == project_id {
                match fallback {
                    Some((pid, tid)) => {
                        runtime.active_project_id = pid;
                        runtime.active_tab_id = tid;
                    }
                    None => {
                        runtime.active_project_id = 0;
                        runtime.active_tab_id = 0;
                    }
                }
                active_changed = true;
            }
        }

        for tab_id in deleted_tab_ids {
            let _ = self.events.send(Event {
                kind: Some(roost_proto::v1::event::Kind::TabDeleted(TabDeletedEvent {
                    tab_id,
                })),
            });
        }
        let _ = self.events.send(Event {
            kind: Some(roost_proto::v1::event::Kind::ProjectDeleted(
                ProjectDeletedEvent { project_id },
            )),
        });
        if active_changed {
            self.emit_active_changed();
        }
        Ok(())
    }

    /// Persist a new sidebar ordering for projects and emit a
    /// `ProjectsReorderedEvent`. The request carries the full target
    /// order; the store's `validate_reorder` enforces id integrity (no
    /// dupes, no unknowns, length matches the project count). Phase 7
    /// commit 3 — Linux UI drag handlers consume this.
    pub fn reorder_projects(&self, ordered_ids: &[i64]) -> Result<(), WorkspaceError> {
        let mut store = self.store.lock().unwrap();
        store.reorder_projects(ordered_ids).map_err(wrap)?;
        drop(store);
        let _ = self.events.send(Event {
            kind: Some(roost_proto::v1::event::Kind::ProjectsReordered(
                ProjectsReorderedEvent {
                    project_ids: ordered_ids.to_vec(),
                },
            )),
        });
        Ok(())
    }

    /// Persist a new tab ordering inside `project_id` and emit a
    /// `TabsReorderedEvent`. Same validation contract as
    /// [`Self::reorder_projects`]; ids that aren't in the project (or
    /// are duplicated, or the length doesn't match) come back as a
    /// `Store` error.
    pub fn reorder_tabs(&self, project_id: i64, ordered_ids: &[i64]) -> Result<(), WorkspaceError> {
        let mut store = self.store.lock().unwrap();
        // Confirm the project exists; map missing to a precise error
        // so callers see ProjectNotFound rather than a generic store
        // error.
        let projects = store.list_projects().map_err(wrap)?;
        if !projects.iter().any(|p| p.id == project_id) {
            return Err(WorkspaceError::ProjectNotFound(project_id));
        }
        store.reorder_tabs(project_id, ordered_ids).map_err(wrap)?;
        drop(store);
        let _ = self.events.send(Event {
            kind: Some(roost_proto::v1::event::Kind::TabsReordered(
                TabsReorderedEvent {
                    project_id,
                    tab_ids: ordered_ids.to_vec(),
                },
            )),
        });
        Ok(())
    }

    pub fn open_tab(
        &self,
        project_id: i64,
        cwd: &str,
        title: &str,
    ) -> Result<StoredTab, WorkspaceError> {
        let store = self.store.lock().unwrap();

        // Confirm the project exists; map missing to a precise error.
        let projects = store.list_projects().map_err(wrap)?;
        if !projects.iter().any(|p| p.id == project_id) {
            return Err(WorkspaceError::ProjectNotFound(project_id));
        }

        let row = store.create_tab(project_id, cwd).map_err(wrap)?;
        let chosen_title = if title.is_empty() {
            derive_title_from_cwd(cwd)
        } else {
            title.to_string()
        };
        if !chosen_title.is_empty() {
            // First write goes through the OSC path so the user_titled
            // lock is preserved (won't ever be set just because we
            // assigned an initial title from the cwd).
            store
                .update_tab_title_if_not_user_set(row.id, &chosen_title)
                .map_err(wrap)?;
        }
        // Re-read so the returned tab reflects the title we just set.
        let row = store.get_tab(row.id).map_err(wrap)?;
        drop(store);

        let mut active_changed = false;
        {
            let mut runtime = self.runtime.lock().unwrap();
            runtime.tabs.insert(row.id, RuntimeTab::default());
            if runtime.active_tab_id == 0 {
                runtime.active_tab_id = row.id;
                runtime.active_project_id = row.project_id;
                active_changed = true;
            }
        }

        let stored = merge_owned(row, RuntimeTab::default());
        // Broadcast the new tab so other UIs converge without polling.
        let runtime = self.runtime.lock().unwrap();
        let proto_tab = merge_tab_from_stored(&stored, &runtime);
        drop(runtime);
        let _ = self.events.send(Event {
            kind: Some(roost_proto::v1::event::Kind::TabOpened(TabOpenedEvent {
                tab: Some(proto_tab),
            })),
        });
        if active_changed {
            self.emit_active_changed();
        }
        Ok(stored)
    }

    pub fn close_tab(&self, tab_id: i64) -> Result<(), WorkspaceError> {
        // Phase 1 — store work: confirm existence, capture the parent
        // project_id, delete the row, then (still under the same store
        // lock) precompute a fallback (project, tab) pair we can
        // promote to active if the tab being closed currently holds
        // that role, plus a flag for whether the project is now empty.
        // The fallback may live in a different project, which is why
        // we capture both fields — leaving `active_project_id` stale
        // would break clients that rely on the `(project, tab)` pair.
        //
        // We compute the fallback unconditionally rather than peeking
        // at `runtime.active_tab_id` first: the lookup is one in-memory
        // SQLite query, and it lets us hold ONLY the store lock during
        // this phase. That preserves the global lock order — `store` is
        // always taken before `runtime`, matching `snapshot()`.
        // Reversing the order in any single method opens a deadlock
        // window against concurrent callers.
        let project_id: i64;
        let fallback: Option<(i64, i64)>;
        let project_now_empty: bool;
        {
            let store = self.store.lock().unwrap();
            let row = store.get_tab(tab_id).map_err(wrap)?;
            project_id = row.project_id;
            store.delete_tab(tab_id).map_err(wrap)?;
            fallback = match store.list_projects() {
                Ok(projects) => projects.into_iter().find_map(|p| {
                    store
                        .list_tabs(p.id)
                        .ok()
                        .and_then(|tabs| tabs.into_iter().next().map(|t| (t.project_id, t.id)))
                }),
                Err(err) => {
                    warn!(
                        tab_id,
                        ?err,
                        "close_tab: list_projects failed while computing fallback"
                    );
                    None
                }
            };
            // Empty-project check while we already hold the store lock.
            // Used below to drive the cascade-to-delete_project — the
            // policy the Go binary's `cmd/roost/app.go:807-808`
            // implements ("when a project's last tab closes, close
            // the project silently"). Doing it here (daemon-side)
            // rather than client-side is what fixes the cross-client
            // safety hole: the Mac UI's local `tabs` list omits
            // headless-CLI-opened tabs, so the client could otherwise
            // delete a project the daemon thinks still has tabs.
            //
            // On a SQLite error we default to "not empty" so the
            // cascade doesn't fire on bad data — better to leave an
            // empty project around than to delete one that still has
            // tabs we just couldn't list.
            project_now_empty = match store.list_tabs(project_id) {
                Ok(tabs) => tabs.is_empty(),
                Err(err) => {
                    warn!(
                        project_id,
                        tab_id,
                        ?err,
                        "close_tab: list_tabs failed while checking project emptiness"
                    );
                    false
                }
            };
        }

        // Phase 2 — runtime work: drop the per-tab entry; if this tab
        // was the active selection, promote the fallback (or zero out).
        let mut active_changed = false;
        {
            let mut runtime = self.runtime.lock().unwrap();
            runtime.tabs.remove(&tab_id);
            if runtime.active_tab_id == tab_id {
                match fallback {
                    Some((p_id, t_id)) => {
                        runtime.active_tab_id = t_id;
                        runtime.active_project_id = p_id;
                    }
                    None => {
                        runtime.active_tab_id = 0;
                        runtime.active_project_id = 0;
                    }
                }
                active_changed = true;
            }
        }

        // Phase 3 — broadcast the tab event before any cascade so the
        // event order on the wire is per-tab `TabDeletedEvent` first,
        // then `ProjectDeletedEvent` if the cascade triggered. Matches
        // the order `delete_project` itself emits for multi-tab
        // deletes.
        let _ = self.events.send(Event {
            kind: Some(roost_proto::v1::event::Kind::TabDeleted(TabDeletedEvent {
                tab_id,
            })),
        });
        if active_changed {
            self.emit_active_changed();
        }

        // Phase 4 — cascade. If the parent project is now empty,
        // delete it. Daemon-authoritative empty-check (vs UI-local)
        // is what makes this safe across clients: a Mac UI tab
        // exiting won't wipe out a project that still has tabs
        // opened headlessly by `roost-cli-rs tab open`.
        // `delete_project` handles its own locking + events. A
        // concurrent delete from another path produces
        // `ProjectNotFound`, which we tolerate silently; any other
        // error is logged so silent SQLite failures don't leave the
        // workspace in a bad state without any signal.
        if project_now_empty {
            if let Err(err) = self.delete_project(project_id) {
                if !matches!(err, WorkspaceError::ProjectNotFound(_)) {
                    warn!(
                        project_id,
                        tab_id,
                        ?err,
                        "close_tab: cascade delete_project failed"
                    );
                }
            }
        }
        Ok(())
    }

    pub fn focus_tab(&self, tab_id: i64) -> Result<(i64, i64), WorkspaceError> {
        let store = self.store.lock().unwrap();
        let row = store.get_tab(tab_id).map_err(wrap)?;
        drop(store);
        let prev;
        let changed;
        {
            let mut runtime = self.runtime.lock().unwrap();
            prev = (runtime.active_project_id, runtime.active_tab_id);
            changed = prev != (row.project_id, row.id);
            runtime.active_project_id = row.project_id;
            runtime.active_tab_id = row.id;
        }
        if changed {
            self.emit_active_changed();
        }
        Ok(prev)
    }

    pub fn set_tab_title(
        &self,
        tab_id: i64,
        title: &str,
        user: bool,
    ) -> Result<(), WorkspaceError> {
        let store = self.store.lock().unwrap();
        let n = if user {
            store.rename_tab_and_lock(tab_id, title).map_err(wrap)?
        } else {
            store
                .update_tab_title_if_not_user_set(tab_id, title)
                .map_err(wrap)?
        };
        if n == 0 {
            // For OSC writes, n=0 means the tab was missing OR the lock
            // is set. Distinguish by re-checking existence; missing => 404,
            // locked => silent no-op (Go semantics).
            if store.get_tab(tab_id).is_err() {
                return Err(WorkspaceError::TabNotFound(tab_id));
            }
            return Ok(());
        }
        let final_title = store.get_tab(tab_id).map_err(wrap)?.title;
        drop(store);
        let _ = self.events.send(Event {
            kind: Some(roost_proto::v1::event::Kind::TabTitle(
                TabTitleChangedEvent {
                    tab_id,
                    title: final_title,
                },
            )),
        });
        Ok(())
    }

    pub fn set_tab_state(&self, tab_id: i64, state: TabState) -> Result<(), WorkspaceError> {
        // TabState is runtime-only; we don't persist it.
        // Confirm the tab exists in the store.
        {
            let store = self.store.lock().unwrap();
            store.get_tab(tab_id).map_err(wrap)?;
        }
        {
            let mut runtime = self.runtime.lock().unwrap();
            let entry = runtime.tabs.entry(tab_id).or_default();
            entry.state = state;
        }
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
        let store = self.store.lock().unwrap();
        store.get_tab(tab_id).map_err(wrap)?;
        store.update_tab_cwd(tab_id, cwd).map_err(wrap)?;
        drop(store);
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
        {
            let store = self.store.lock().unwrap();
            store.get_tab(tab_id).map_err(wrap)?;
        }
        {
            let mut runtime = self.runtime.lock().unwrap();
            let entry = runtime.tabs.entry(tab_id).or_default();
            entry.has_notification = has_pending;
        }
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
        {
            let store = self.store.lock().unwrap();
            store.get_tab(tab_id).map_err(wrap)?;
        }
        {
            let mut runtime = self.runtime.lock().unwrap();
            let entry = runtime.tabs.entry(tab_id).or_default();
            entry.hook_active = active;
        }
        let _ = self.events.send(Event {
            kind: Some(roost_proto::v1::event::Kind::HookActive(
                HookActiveChangedEvent { tab_id, active },
            )),
        });
        Ok(())
    }

    /// Snapshot the current `(active_project_id, active_tab_id)` and emit
    /// an `ActiveChangedEvent`. Pulled out so the mutators above can
    /// always emit consistently without re-locking-and-reading inline.
    fn emit_active_changed(&self) {
        let (project_id, tab_id) = self.active();
        let _ = self.events.send(Event {
            kind: Some(roost_proto::v1::event::Kind::Active(ActiveChangedEvent {
                project_id,
                tab_id,
            })),
        });
    }

    pub fn fire_notification(
        &self,
        tab_id: i64,
        title: &str,
        body: &str,
    ) -> Result<(), WorkspaceError> {
        // set_tab_notification confirms existence and emits TabNotification.
        self.set_tab_notification(tab_id, true)?;
        let _ = self.events.send(Event {
            kind: Some(roost_proto::v1::event::Kind::Notification(
                NotificationEvent {
                    tab_id,
                    title: title.to_string(),
                    body: body.to_string(),
                },
            )),
        });
        Ok(())
    }

    /// Reports a `tabs_reordered` and a `projects_reordered` event for the
    /// current snapshot. Useful when a freshly-attached client wants a
    /// resync without an explicit `ListTabs` call.
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
        let store = self.store.lock().unwrap();
        let row = store.get_tab(tab_id).ok()?;
        drop(store);
        let runtime = self.runtime.lock().unwrap();
        let rt = runtime.tabs.get(&tab_id).copied().unwrap_or_default();
        Some(merge_owned(row, rt))
    }
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}

fn merge_tab(row: crate::store::TabRow, runtime: &RuntimeState) -> Tab {
    let rt = runtime.tabs.get(&row.id).copied().unwrap_or_default();
    Tab {
        id: row.id,
        project_id: row.project_id,
        title: row.title,
        cwd: row.cwd,
        state: rt.state as i32,
        has_notification: rt.has_notification,
        is_active: runtime.active_tab_id == row.id,
        user_titled: row.user_titled,
        position: row.position,
        created_at: row.created_at,
        last_active: row.last_active,
        hook_active: rt.hook_active,
    }
}

fn merge_owned(row: crate::store::TabRow, rt: RuntimeTab) -> StoredTab {
    StoredTab {
        id: row.id,
        project_id: row.project_id,
        title: row.title,
        cwd: row.cwd,
        state: rt.state,
        has_notification: rt.has_notification,
        user_titled: row.user_titled,
        position: row.position,
        created_at: row.created_at,
        last_active: row.last_active,
        hook_active: rt.hook_active,
    }
}

/// Build a proto `Tab` from a `StoredTab` (which already has runtime
/// fields merged) by additionally tagging the `is_active` flag from the
/// current selection. Used by `open_tab` to broadcast a `TabOpenedEvent`.
fn merge_tab_from_stored(stored: &StoredTab, runtime: &RuntimeState) -> Tab {
    Tab {
        id: stored.id,
        project_id: stored.project_id,
        title: stored.title.clone(),
        cwd: stored.cwd.clone(),
        state: stored.state as i32,
        has_notification: stored.has_notification,
        is_active: runtime.active_tab_id == stored.id,
        user_titled: stored.user_titled,
        position: stored.position,
        created_at: stored.created_at,
        last_active: stored.last_active,
        hook_active: stored.hook_active,
    }
}

fn derive_title_from_cwd(cwd: &str) -> String {
    std::path::Path::new(cwd)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| cwd.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_close_tab_cascades_to_empty_project() {
        // Closing the only tab in a project cascades to delete the
        // parent project itself (M5 of goal-mac-polish-cursor-keys).
        // Matches Go binary's cmd/roost/app.go:807-808 ("when a
        // project's last tab closes, close the project silently").
        let ws = Workspace::new();
        let project = ws.ensure_default_project("/tmp");
        let tab = ws.open_tab(project, "/tmp/work", "").unwrap();
        let snap = ws.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].tabs.len(), 1);
        ws.close_tab(tab.id).unwrap();
        // Cascade: project itself is gone now since it had no
        // remaining tabs.
        assert!(ws.snapshot().is_empty());
    }

    #[test]
    fn close_one_of_many_tabs_keeps_project() {
        // The cascade only fires when the project becomes empty.
        // Closing one tab in a multi-tab project leaves the project
        // (and its remaining tabs) in place.
        let ws = Workspace::new();
        let project = ws.ensure_default_project("/tmp");
        let tab_a = ws.open_tab(project, "/tmp/a", "").unwrap();
        let _tab_b = ws.open_tab(project, "/tmp/b", "").unwrap();
        ws.close_tab(tab_a.id).unwrap();
        let snap = ws.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].tabs.len(), 1);
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

    #[test]
    fn create_project_assigns_untitled_when_name_empty() {
        let ws = Workspace::new();
        let p1 = ws.create_project("", "/tmp").unwrap();
        let p2 = ws.create_project("", "/tmp").unwrap();
        let p3 = ws.create_project("named", "/tmp").unwrap();
        assert_eq!(p1.name, "Untitled 1");
        assert_eq!(p2.name, "Untitled 2");
        assert_eq!(p3.name, "named");
    }

    #[test]
    fn rename_project_emits_event_and_persists() {
        let ws = Workspace::new();
        let mut rx = ws.subscribe();
        let p = ws.create_project("orig", "/tmp").unwrap();
        // Drain the ProjectCreated event so we observe the rename one.
        let _ = rx.try_recv();
        ws.rename_project(p.id, "renamed").unwrap();
        let snap = ws.snapshot();
        assert_eq!(snap.iter().find(|x| x.id == p.id).unwrap().name, "renamed");
        match rx.try_recv() {
            Ok(Event {
                kind: Some(roost_proto::v1::event::Kind::ProjectRenamed(e)),
            }) => {
                assert_eq!(e.project_id, p.id);
                assert_eq!(e.name, "renamed");
            }
            other => panic!("expected ProjectRenamed, got {other:?}"),
        }
    }

    #[test]
    fn delete_project_cascades_tabs_and_emits_events() {
        let ws = Workspace::new();
        let p = ws.create_project("doomed", "/tmp").unwrap();
        let t1 = ws.open_tab(p.id, "/tmp", "").unwrap();
        let t2 = ws.open_tab(p.id, "/tmp", "").unwrap();

        let mut rx = ws.subscribe();
        ws.delete_project(p.id).unwrap();

        // Snapshot reflects the deletion.
        assert!(ws.snapshot().iter().all(|x| x.id != p.id));

        // Event order: TabDeleted x2 then ProjectDeleted.
        let mut tab_deleted_ids = Vec::new();
        let mut project_deleted = false;
        while let Ok(ev) = rx.try_recv() {
            match ev.kind {
                Some(roost_proto::v1::event::Kind::TabDeleted(e)) => {
                    tab_deleted_ids.push(e.tab_id);
                }
                Some(roost_proto::v1::event::Kind::ProjectDeleted(e)) => {
                    assert_eq!(e.project_id, p.id);
                    // TabDeleted must arrive before ProjectDeleted per the
                    // contract documented on `delete_project`.
                    assert_eq!(tab_deleted_ids.len(), 2);
                    project_deleted = true;
                }
                _ => {}
            }
        }
        assert!(tab_deleted_ids.contains(&t1.id));
        assert!(tab_deleted_ids.contains(&t2.id));
        assert!(project_deleted);
    }

    #[test]
    fn delete_project_promotes_fallback_active_selection() {
        let ws = Workspace::new();
        let keeper = ws.create_project("keeper", "/tmp").unwrap();
        let keep_tab = ws.open_tab(keeper.id, "/tmp", "").unwrap();
        let doomed = ws.create_project("doomed", "/tmp").unwrap();
        let _doomed_tab = ws.open_tab(doomed.id, "/tmp", "").unwrap();
        // Force active onto the doomed project.
        ws.focus_tab(_doomed_tab.id).unwrap();
        assert_eq!(ws.active(), (doomed.id, _doomed_tab.id));

        ws.delete_project(doomed.id).unwrap();
        // Active selection must have moved to the keeper's tab.
        assert_eq!(ws.active(), (keeper.id, keep_tab.id));
    }

    #[test]
    fn delete_project_unknown_returns_not_found() {
        let ws = Workspace::new();
        let err = ws.delete_project(999).unwrap_err();
        match err {
            WorkspaceError::ProjectNotFound(id) => assert_eq!(id, 999),
            other => panic!("expected ProjectNotFound, got {other:?}"),
        }
    }

    #[test]
    fn projects_survive_reopen_tabs_do_not() {
        // Projects persist across daemon restarts; tabs don't.
        // A Tab row implies a live PTY, and PTYs die with the daemon,
        // so `Workspace::open` purges all tab rows at startup
        // (commit on goal-mac-polish-cursor-keys follow-ups).
        // Without that, M5's empty-project cascade misfires on the
        // user's first `exit` because the daemon thinks orphan tabs
        // still exist.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ws.db");

        let project_name: String = {
            let ws = Workspace::open(&path).unwrap();
            let project_id = ws.ensure_default_project("/tmp/work");
            ws.open_tab(project_id, "/tmp/work", "first").unwrap();
            let snap = ws.snapshot();
            assert_eq!(snap.len(), 1);
            assert_eq!(snap[0].tabs.len(), 1);
            snap[0].name.clone()
        };

        // Reopen — projects survive (we want the user's named
        // workspaces back), but tabs are wiped clean.
        let ws = Workspace::open(&path).unwrap();
        let snap = ws.snapshot();
        assert_eq!(snap.len(), 1, "project should persist across reopen");
        assert_eq!(
            snap[0].name, project_name,
            "project name should match the original"
        );
        assert!(
            snap[0].tabs.is_empty(),
            "orphan tabs should be purged on Workspace::open"
        );
    }

    #[test]
    fn reorder_projects_persists_and_emits_event() {
        let ws = Workspace::new();
        let p1 = ws.create_project("first", "/tmp/a").unwrap();
        let p2 = ws.create_project("second", "/tmp/b").unwrap();
        let p3 = ws.create_project("third", "/tmp/c").unwrap();

        let mut rx = ws.subscribe();
        // New order: 3, 1, 2
        ws.reorder_projects(&[p3.id, p1.id, p2.id]).unwrap();

        let snap = ws.snapshot();
        let observed: Vec<i64> = snap.iter().map(|p| p.id).collect();
        assert_eq!(observed, vec![p3.id, p1.id, p2.id]);

        match rx.try_recv() {
            Ok(Event {
                kind: Some(roost_proto::v1::event::Kind::ProjectsReordered(e)),
            }) => assert_eq!(e.project_ids, vec![p3.id, p1.id, p2.id]),
            other => panic!("expected ProjectsReordered, got {other:?}"),
        }
    }

    #[test]
    fn reorder_tabs_persists_and_emits_event() {
        let ws = Workspace::new();
        let p = ws.create_project("p", "/tmp").unwrap();
        let t1 = ws.open_tab(p.id, "/tmp", "").unwrap();
        let t2 = ws.open_tab(p.id, "/tmp", "").unwrap();
        let t3 = ws.open_tab(p.id, "/tmp", "").unwrap();

        let mut rx = ws.subscribe();
        ws.reorder_tabs(p.id, &[t3.id, t1.id, t2.id]).unwrap();

        let snap = ws.snapshot();
        let proj = snap.iter().find(|x| x.id == p.id).unwrap();
        let observed: Vec<i64> = proj.tabs.iter().map(|t| t.id).collect();
        assert_eq!(observed, vec![t3.id, t1.id, t2.id]);

        match rx.try_recv() {
            Ok(Event {
                kind: Some(roost_proto::v1::event::Kind::TabsReordered(e)),
            }) => {
                assert_eq!(e.project_id, p.id);
                assert_eq!(e.tab_ids, vec![t3.id, t1.id, t2.id]);
            }
            other => panic!("expected TabsReordered, got {other:?}"),
        }
    }

    #[test]
    fn reorder_tabs_rejects_unknown_project() {
        let ws = Workspace::new();
        let err = ws.reorder_tabs(9999, &[]).unwrap_err();
        assert!(matches!(err, WorkspaceError::ProjectNotFound(9999)));
    }

    #[test]
    fn reorder_projects_rejects_wrong_count() {
        let ws = Workspace::new();
        let _ = ws.create_project("a", "").unwrap();
        let _ = ws.create_project("b", "").unwrap();
        // Only 1 id supplied; store validates the count and rejects.
        let err = ws.reorder_projects(&[1]).unwrap_err();
        assert!(matches!(err, WorkspaceError::Store(_)));
    }
}
