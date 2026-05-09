//! SQLite-backed persistence for projects + tabs.
//!
//! Mirrors `internal/store/store.go` semantically. Migrations are embedded
//! at compile time via `include_str!` and ship with the binary; the schema
//! itself is byte-for-byte identical to the Go side so a user with an
//! existing `roost.db` can point a new build at it without a wipe.
//!
//! Threading: `rusqlite::Connection` is `Send` but not `Sync`. The store
//! wraps a single connection in a `Mutex` and serialises access. SQLite's
//! own busy_timeout handles WAL contention if a future revision adds a
//! pool.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OpenFlags};
use thiserror::Error;
use tracing::debug;

/// Embedded migrations. Bytes-identical to internal/store/migrations/.
/// New migrations append to this list; never reorder or rewrite existing
/// entries — they're applied in version order and recorded in
/// `schema_migrations`.
const MIGRATIONS: &[(u32, &str, &str)] = &[
    (1, "init", include_str!("../migrations/0001_init.sql")),
    (
        2,
        "user_titled",
        include_str!("../migrations/0002_user_titled.sql"),
    ),
];

/// One persisted project row. Mirrors the Go `store.Project` struct.
#[derive(Clone, Debug)]
pub struct ProjectRow {
    pub id: i64,
    pub name: String,
    pub cwd: String,
    pub position: i32,
    pub created_at: i64,
}

/// One persisted tab row. Mirrors the Go `store.Tab` struct.
#[derive(Clone, Debug)]
pub struct TabRow {
    pub id: i64,
    pub project_id: i64,
    pub title: String,
    pub cwd: String,
    pub last_command: String,
    pub position: i32,
    pub created_at: i64,
    pub last_active: i64,
    pub user_titled: bool,
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("project {0} not found")]
    ProjectNotFound(i64),
    #[error("tab {0} not found")]
    TabNotFound(i64),
    #[error("reorder: expected {expected} ids, got {got}")]
    ReorderCountMismatch { expected: usize, got: usize },
    #[error("reorder: id {0} is unknown for this scope")]
    ReorderUnknownId(i64),
    #[error("reorder: id {0} appeared more than once")]
    ReorderDuplicateId(i64),
    #[error("sqlite: {0}")]
    Sql(#[from] rusqlite::Error),
}

pub type StoreResult<T> = Result<T, StoreError>;

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open a SQLite database at `path`, creating it (and applying
    /// migrations) if needed. Use [`Store::in_memory`] for tests.
    pub fn open(path: impl AsRef<Path>) -> StoreResult<Self> {
        let conn = Connection::open_with_flags(
            path.as_ref(),
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )?;
        Self::configure_pragmas(&conn)?;
        let mut store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    /// Open an in-memory SQLite database. Useful for tests; the schema
    /// is migrated immediately so the returned store is fully usable.
    pub fn in_memory() -> StoreResult<Self> {
        let conn = Connection::open_in_memory()?;
        Self::configure_pragmas(&conn)?;
        let mut store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn configure_pragmas(conn: &Connection) -> StoreResult<()> {
        // Mirrors the Go side's DSN-encoded pragmas. WAL gives a more
        // responsive single-writer setup; foreign_keys must be enabled
        // per-connection for ON DELETE CASCADE on `tab` to fire.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;\
             PRAGMA foreign_keys = ON;\
             PRAGMA busy_timeout = 5000;",
        )?;
        Ok(())
    }

    fn migrate(&mut self) -> StoreResult<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (\
                 version INTEGER PRIMARY KEY,\
                 applied_at INTEGER NOT NULL\
             )",
        )?;

        let mut applied = std::collections::HashSet::<u32>::new();
        let mut stmt = self.conn.prepare("SELECT version FROM schema_migrations")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            applied.insert(row.get::<_, u32>(0)?);
        }
        drop(rows);
        drop(stmt);

        for (version, name, sql) in MIGRATIONS {
            if applied.contains(version) {
                continue;
            }
            debug!(version, name, "applying migration");
            let tx = self.conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.execute(
                "INSERT INTO schema_migrations(version, applied_at) VALUES (?, ?)",
                params![version, now_secs()],
            )?;
            tx.commit()?;
        }

        Ok(())
    }

    // ----- Projects -------------------------------------------------------

    pub fn create_project(&self, name: &str, cwd: &str) -> StoreResult<ProjectRow> {
        let now = now_secs();
        let pos: i32 = self.conn.query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM project",
            [],
            |row| row.get(0),
        )?;
        self.conn.execute(
            "INSERT INTO project(name, cwd, position, created_at) VALUES (?, ?, ?, ?)",
            params![name, cwd, pos, now],
        )?;
        let id = self.conn.last_insert_rowid();
        Ok(ProjectRow {
            id,
            name: name.to_string(),
            cwd: cwd.to_string(),
            position: pos,
            created_at: now,
        })
    }

    pub fn list_projects(&self) -> StoreResult<Vec<ProjectRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, cwd, position, created_at FROM project ORDER BY position")?;
        let mut out = Vec::new();
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            out.push(ProjectRow {
                id: row.get(0)?,
                name: row.get(1)?,
                cwd: row.get(2)?,
                position: row.get(3)?,
                created_at: row.get(4)?,
            });
        }
        Ok(out)
    }

    pub fn rename_project(&self, id: i64, name: &str) -> StoreResult<()> {
        let n = self.conn.execute(
            "UPDATE project SET name = ? WHERE id = ?",
            params![name, id],
        )?;
        if n == 0 {
            return Err(StoreError::ProjectNotFound(id));
        }
        Ok(())
    }

    pub fn delete_project(&self, id: i64) -> StoreResult<()> {
        self.conn
            .execute("DELETE FROM project WHERE id = ?", params![id])?;
        Ok(())
    }

    pub fn reorder_projects(&mut self, ordered_ids: &[i64]) -> StoreResult<()> {
        let tx = self.conn.transaction()?;
        let existing = collect_ids(&tx, "SELECT id FROM project", [])?;
        validate_reorder(ordered_ids, &existing)?;

        {
            let mut stmt = tx.prepare("UPDATE project SET position = ? WHERE id = ?")?;
            for (i, id) in ordered_ids.iter().enumerate() {
                stmt.execute(params![i as i32, id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ----- Tabs -----------------------------------------------------------

    pub fn create_tab(&self, project_id: i64, cwd: &str) -> StoreResult<TabRow> {
        let now = now_secs();
        let pos: i32 = self.conn.query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM tab WHERE project_id = ?",
            params![project_id],
            |row| row.get(0),
        )?;
        self.conn.execute(
            "INSERT INTO tab(project_id, cwd, position, created_at, last_active) \
             VALUES (?, ?, ?, ?, ?)",
            params![project_id, cwd, pos, now, now],
        )?;
        let id = self.conn.last_insert_rowid();
        Ok(TabRow {
            id,
            project_id,
            title: String::new(),
            cwd: cwd.to_string(),
            last_command: String::new(),
            position: pos,
            created_at: now,
            last_active: now,
            user_titled: false,
        })
    }

    pub fn get_tab(&self, id: i64) -> StoreResult<TabRow> {
        self.conn
            .query_row(
                "SELECT id, project_id, COALESCE(title,''), cwd, COALESCE(last_command,''), \
                        position, created_at, last_active, user_titled \
                 FROM tab WHERE id = ?",
                params![id],
                row_to_tab,
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => StoreError::TabNotFound(id),
                other => StoreError::Sql(other),
            })
    }

    pub fn list_tabs(&self, project_id: i64) -> StoreResult<Vec<TabRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, COALESCE(title,''), cwd, COALESCE(last_command,''), \
                    position, created_at, last_active, user_titled \
             FROM tab WHERE project_id = ? ORDER BY position",
        )?;
        let mut out = Vec::new();
        let mut rows = stmt.query(params![project_id])?;
        while let Some(row) = rows.next()? {
            out.push(row_to_tab(row)?);
        }
        Ok(out)
    }

    pub fn update_tab_cwd(&self, id: i64, cwd: &str) -> StoreResult<()> {
        self.conn
            .execute("UPDATE tab SET cwd = ? WHERE id = ?", params![cwd, id])?;
        Ok(())
    }

    /// Atomic write of a manual rename. Sets both `title` and `user_titled = 1`
    /// in a single UPDATE so an interleaved
    /// `update_tab_title_if_not_user_set` cannot lose. Returns rows-affected:
    /// 0 means the tab is missing.
    pub fn rename_tab_and_lock(&self, id: i64, title: &str) -> StoreResult<usize> {
        let n = self.conn.execute(
            "UPDATE tab SET title = ?, user_titled = 1 WHERE id = ?",
            params![title, id],
        )?;
        Ok(n)
    }

    /// OSC 1/2 path: write the title only when the tab is not user-locked.
    /// Returns rows-affected so the caller can distinguish "applied" (1)
    /// from "suppressed by lock or missing tab" (0).
    pub fn update_tab_title_if_not_user_set(&self, id: i64, title: &str) -> StoreResult<usize> {
        let n = self.conn.execute(
            "UPDATE tab SET title = ? WHERE id = ? AND user_titled = 0",
            params![title, id],
        )?;
        Ok(n)
    }

    pub fn touch_tab(&self, id: i64) -> StoreResult<()> {
        self.conn.execute(
            "UPDATE tab SET last_active = ? WHERE id = ?",
            params![now_secs(), id],
        )?;
        Ok(())
    }

    pub fn delete_tab(&self, id: i64) -> StoreResult<()> {
        self.conn
            .execute("DELETE FROM tab WHERE id = ?", params![id])?;
        Ok(())
    }

    pub fn reorder_tabs(&mut self, project_id: i64, ordered_ids: &[i64]) -> StoreResult<()> {
        let tx = self.conn.transaction()?;
        let existing = collect_ids(
            &tx,
            "SELECT id FROM tab WHERE project_id = ?",
            params![project_id],
        )?;
        validate_reorder(ordered_ids, &existing)?;

        {
            let mut stmt =
                tx.prepare("UPDATE tab SET position = ? WHERE id = ? AND project_id = ?")?;
            for (i, id) in ordered_ids.iter().enumerate() {
                stmt.execute(params![i as i32, id, project_id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

fn row_to_tab(row: &rusqlite::Row<'_>) -> rusqlite::Result<TabRow> {
    let user_titled: i64 = row.get(8)?;
    Ok(TabRow {
        id: row.get(0)?,
        project_id: row.get(1)?,
        title: row.get(2)?,
        cwd: row.get(3)?,
        last_command: row.get(4)?,
        position: row.get(5)?,
        created_at: row.get(6)?,
        last_active: row.get(7)?,
        user_titled: user_titled != 0,
    })
}

fn collect_ids<P: rusqlite::Params>(
    conn: &impl std::ops::Deref<Target = Connection>,
    sql: &str,
    params: P,
) -> StoreResult<std::collections::HashSet<i64>> {
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query(params)?;
    let mut out = std::collections::HashSet::new();
    while let Some(row) = rows.next()? {
        out.insert(row.get(0)?);
    }
    Ok(out)
}

fn validate_reorder(ordered: &[i64], existing: &std::collections::HashSet<i64>) -> StoreResult<()> {
    if ordered.len() != existing.len() {
        return Err(StoreError::ReorderCountMismatch {
            expected: existing.len(),
            got: ordered.len(),
        });
    }
    let mut seen = std::collections::HashSet::new();
    for id in ordered {
        if !existing.contains(id) {
            return Err(StoreError::ReorderUnknownId(*id));
        }
        if !seen.insert(*id) {
            return Err(StoreError::ReorderDuplicateId(*id));
        }
    }
    Ok(())
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_apply_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");

        let store = Store::open(&path).unwrap();
        drop(store);
        // Reopening should be a no-op for migrations and not fail.
        let _ = Store::open(&path).unwrap();
    }

    #[test]
    fn create_project_and_tab_round_trip() {
        let store = Store::in_memory().unwrap();
        let project = store.create_project("Roost", "/tmp/roost").unwrap();
        assert_eq!(project.position, 0);

        let tab = store.create_tab(project.id, "/tmp/roost/work").unwrap();
        assert_eq!(tab.position, 0);
        assert_eq!(tab.cwd, "/tmp/roost/work");
        assert!(!tab.user_titled);

        let tabs = store.list_tabs(project.id).unwrap();
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].id, tab.id);
    }

    #[test]
    fn user_titled_locks_against_osc() {
        let store = Store::in_memory().unwrap();
        let project = store.create_project("p", "/tmp").unwrap();
        let tab = store.create_tab(project.id, "/tmp").unwrap();

        // Manual rename.
        let n = store.rename_tab_and_lock(tab.id, "manual").unwrap();
        assert_eq!(n, 1);

        // OSC write must not overwrite a locked title.
        let n = store
            .update_tab_title_if_not_user_set(tab.id, "from-osc")
            .unwrap();
        assert_eq!(n, 0);

        let after = store.get_tab(tab.id).unwrap();
        assert_eq!(after.title, "manual");
        assert!(after.user_titled);
    }

    #[test]
    fn cascade_delete_project_removes_tabs() {
        let store = Store::in_memory().unwrap();
        let p1 = store.create_project("p1", "/a").unwrap();
        let _t1 = store.create_tab(p1.id, "/a/x").unwrap();
        let _t2 = store.create_tab(p1.id, "/a/y").unwrap();
        assert_eq!(store.list_tabs(p1.id).unwrap().len(), 2);

        store.delete_project(p1.id).unwrap();
        assert_eq!(store.list_tabs(p1.id).unwrap().len(), 0);
    }

    #[test]
    fn reorder_tabs_renumbers_positions() {
        let mut store = Store::in_memory().unwrap();
        let p = store.create_project("p", "/tmp").unwrap();
        let a = store.create_tab(p.id, "/a").unwrap();
        let b = store.create_tab(p.id, "/b").unwrap();
        let c = store.create_tab(p.id, "/c").unwrap();

        store.reorder_tabs(p.id, &[c.id, a.id, b.id]).unwrap();

        let tabs = store.list_tabs(p.id).unwrap();
        assert_eq!(
            tabs.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![c.id, a.id, b.id]
        );
        assert_eq!(
            tabs.iter().map(|t| t.position).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn reorder_tabs_rejects_mismatch() {
        let mut store = Store::in_memory().unwrap();
        let p = store.create_project("p", "/tmp").unwrap();
        let a = store.create_tab(p.id, "/a").unwrap();
        let _b = store.create_tab(p.id, "/b").unwrap();

        let err = store.reorder_tabs(p.id, &[a.id]).unwrap_err();
        assert!(matches!(err, StoreError::ReorderCountMismatch { .. }));
    }
}
