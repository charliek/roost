// Package store persists Roost's project + tab state to SQLite.
//
// The schema is small and stable: a project has many tabs, tabs reference
// their parent project. All persistence happens through the *Store value
// returned by Open. Methods return concrete domain values (not rows) so
// callers don't need to import database/sql.
package store

import (
	"database/sql"
	"embed"
	"errors"
	"fmt"
	"io/fs"
	"sort"
	"time"

	_ "modernc.org/sqlite"
)

//go:embed migrations/*.sql
var migrationsFS embed.FS

// Project is a sidebar entry — a working group with a name and default cwd.
type Project struct {
	ID        int64
	Name      string
	CWD       string
	Position  int
	CreatedAt time.Time
}

// Tab is one terminal inside a project.
type Tab struct {
	ID          int64
	ProjectID   int64
	Title       string // empty if not set
	CWD         string
	LastCommand string // last command the user ran in this tab; empty if none
	Position    int
	CreatedAt   time.Time
	LastActive  time.Time
	// UserTitled is true once the user explicitly renamed the tab (Cmd-R
	// popover or `roost-cli set-title`). While set, OSC 1/2 writes from
	// the shell are suppressed by UpdateTabTitleIfNotUserSet. v1 has no
	// in-app way to clear the lock.
	UserTitled bool
}

// Store is the SQLite handle and migration state. database/sql's
// connection pool plus SQLite's own locking make the methods below
// goroutine-safe for individual statements. Multi-statement sequences
// (e.g. CreateProject's SELECT MAX(position) + INSERT) are not atomic
// and should be called from a single goroutine — today that's the GTK
// main thread.
type Store struct {
	db *sql.DB
}

// Open initializes the SQLite database at path, applying any pending
// migrations. Creates the file if it doesn't exist. Caller must Close.
func Open(path string) (*Store, error) {
	// modernc/sqlite uses a "?_pragma=" URL fragment for pragmas. WAL
	// gives us a more responsive single-writer setup; foreign_keys
	// must be enabled per-connection for the ON DELETE CASCADE to fire.
	dsn := path + "?_pragma=journal_mode(WAL)&_pragma=foreign_keys(1)&_pragma=busy_timeout(5000)"
	db, err := sql.Open("sqlite", dsn)
	if err != nil {
		return nil, fmt.Errorf("sql.Open: %w", err)
	}
	if err := db.Ping(); err != nil {
		_ = db.Close()
		return nil, fmt.Errorf("ping: %w", err)
	}
	s := &Store{db: db}
	if err := s.migrate(); err != nil {
		_ = db.Close()
		return nil, err
	}
	return s, nil
}

// Close releases the underlying connection.
func (s *Store) Close() error { return s.db.Close() }

func (s *Store) migrate() error {
	if _, err := s.db.Exec(`CREATE TABLE IF NOT EXISTS schema_migrations (
		version INTEGER PRIMARY KEY,
		applied_at INTEGER NOT NULL
	)`); err != nil {
		return fmt.Errorf("create schema_migrations: %w", err)
	}

	applied, err := s.loadAppliedMigrations()
	if err != nil {
		return err
	}

	files, err := fs.Glob(migrationsFS, "migrations/*.sql")
	if err != nil {
		return err
	}
	sort.Strings(files)

	for _, path := range files {
		base := path[len("migrations/"):] // strip dir prefix
		if len(base) < 5 || base[4] != '_' {
			return fmt.Errorf("migration filename %q does not match NNNN_name.sql", path)
		}
		var version int
		if _, err := fmt.Sscanf(base[:4], "%d", &version); err != nil {
			return fmt.Errorf("migration filename %q has non-numeric version: %w", path, err)
		}
		if applied[version] {
			continue
		}
		body, err := migrationsFS.ReadFile(path)
		if err != nil {
			return err
		}
		tx, err := s.db.Begin()
		if err != nil {
			return err
		}
		if _, err := tx.Exec(string(body)); err != nil {
			_ = tx.Rollback()
			return fmt.Errorf("apply %s: %w", path, err)
		}
		if _, err := tx.Exec(`INSERT INTO schema_migrations(version, applied_at) VALUES (?, ?)`,
			version, time.Now().Unix()); err != nil {
			_ = tx.Rollback()
			return err
		}
		if err := tx.Commit(); err != nil {
			return err
		}
	}
	return nil
}

// loadAppliedMigrations reads the schema_migrations table into a set
// of applied versions. Errors during iteration are surfaced (via
// rows.Err) so a transient driver failure can't masquerade as a clean
// "no migrations applied" result, which would re-run them all.
func (s *Store) loadAppliedMigrations() (map[int]bool, error) {
	applied := map[int]bool{}
	rows, err := s.db.Query(`SELECT version FROM schema_migrations`)
	if err != nil {
		return nil, fmt.Errorf("query migrations: %w", err)
	}
	defer rows.Close()
	for rows.Next() {
		var v int
		if err := rows.Scan(&v); err != nil {
			return nil, fmt.Errorf("scan schema_migrations: %w", err)
		}
		applied[v] = true
	}
	if err := rows.Err(); err != nil {
		return nil, fmt.Errorf("iterate schema_migrations: %w", err)
	}
	return applied, nil
}

// CreateProject inserts a new project. Position is auto-assigned to the
// end of the list.
func (s *Store) CreateProject(name, cwd string) (Project, error) {
	now := time.Now()
	var pos int
	if err := s.db.QueryRow(`SELECT COALESCE(MAX(position), -1) + 1 FROM project`).Scan(&pos); err != nil {
		return Project{}, err
	}
	res, err := s.db.Exec(
		`INSERT INTO project(name, cwd, position, created_at) VALUES (?, ?, ?, ?)`,
		name, cwd, pos, now.Unix())
	if err != nil {
		return Project{}, err
	}
	id, _ := res.LastInsertId()
	return Project{ID: id, Name: name, CWD: cwd, Position: pos, CreatedAt: now}, nil
}

// ListProjects returns all projects ordered by sidebar position.
func (s *Store) ListProjects() ([]Project, error) {
	rows, err := s.db.Query(`SELECT id, name, cwd, position, created_at FROM project ORDER BY position`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []Project
	for rows.Next() {
		var p Project
		var ts int64
		if err := rows.Scan(&p.ID, &p.Name, &p.CWD, &p.Position, &ts); err != nil {
			return nil, err
		}
		p.CreatedAt = time.Unix(ts, 0)
		out = append(out, p)
	}
	return out, rows.Err()
}

// RenameProject updates the project's display name.
func (s *Store) RenameProject(id int64, name string) error {
	res, err := s.db.Exec(`UPDATE project SET name = ? WHERE id = ?`, name, id)
	if err != nil {
		return err
	}
	if n, _ := res.RowsAffected(); n == 0 {
		return errProjectNotFound
	}
	return nil
}

// DeleteProject removes the project and (via ON DELETE CASCADE) its tabs.
func (s *Store) DeleteProject(id int64) error {
	_, err := s.db.Exec(`DELETE FROM project WHERE id = ?`, id)
	return err
}

// CreateTab inserts a tab inside a project. Position is auto-assigned to
// the end of the project's tab list.
func (s *Store) CreateTab(projectID int64, cwd string) (Tab, error) {
	now := time.Now()
	var pos int
	if err := s.db.QueryRow(
		`SELECT COALESCE(MAX(position), -1) + 1 FROM tab WHERE project_id = ?`,
		projectID).Scan(&pos); err != nil {
		return Tab{}, err
	}
	res, err := s.db.Exec(
		`INSERT INTO tab(project_id, cwd, position, created_at, last_active)
		 VALUES (?, ?, ?, ?, ?)`,
		projectID, cwd, pos, now.Unix(), now.Unix())
	if err != nil {
		return Tab{}, err
	}
	id, _ := res.LastInsertId()
	return Tab{
		ID: id, ProjectID: projectID, CWD: cwd, Position: pos,
		CreatedAt: now, LastActive: now,
	}, nil
}

// GetTab loads a single tab by id. Returns sql.ErrNoRows if missing.
func (s *Store) GetTab(id int64) (Tab, error) {
	row := s.db.QueryRow(
		`SELECT id, project_id, COALESCE(title,''), cwd, COALESCE(last_command,''),
		        position, created_at, last_active, user_titled
		 FROM tab WHERE id = ?`, id)
	var t Tab
	var created, active int64
	var userTitled int
	if err := row.Scan(
		&t.ID, &t.ProjectID, &t.Title, &t.CWD, &t.LastCommand,
		&t.Position, &created, &active, &userTitled,
	); err != nil {
		return Tab{}, err
	}
	t.CreatedAt = time.Unix(created, 0)
	t.LastActive = time.Unix(active, 0)
	t.UserTitled = userTitled != 0
	return t, nil
}

// ListTabs returns every tab belonging to a project, ordered by tab position.
func (s *Store) ListTabs(projectID int64) ([]Tab, error) {
	rows, err := s.db.Query(
		`SELECT id, project_id, COALESCE(title,''), cwd, COALESCE(last_command,''),
		        position, created_at, last_active, user_titled
		 FROM tab WHERE project_id = ? ORDER BY position`,
		projectID)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []Tab
	for rows.Next() {
		var t Tab
		var created, active int64
		var userTitled int
		if err := rows.Scan(
			&t.ID, &t.ProjectID, &t.Title, &t.CWD, &t.LastCommand,
			&t.Position, &created, &active, &userTitled,
		); err != nil {
			return nil, err
		}
		t.CreatedAt = time.Unix(created, 0)
		t.LastActive = time.Unix(active, 0)
		t.UserTitled = userTitled != 0
		out = append(out, t)
	}
	return out, rows.Err()
}

// UpdateTabCWD sets the tab's recorded cwd (e.g. on OSC 7 cwd reports).
func (s *Store) UpdateTabCWD(id int64, cwd string) error {
	_, err := s.db.Exec(`UPDATE tab SET cwd = ? WHERE id = ?`, cwd, id)
	return err
}

// UpdateTabTitle sets the tab's display title unconditionally. Used
// only by the test suite today; production callers go through
// RenameTabAndLock (user intent) or UpdateTabTitleIfNotUserSet (OSC).
func (s *Store) UpdateTabTitle(id int64, title string) error {
	_, err := s.db.Exec(`UPDATE tab SET title = ? WHERE id = ?`, title, id)
	return err
}

// RenameTabAndLock writes the user-chosen title and flips the
// user-titled lock in a single atomic UPDATE. Doing both in one
// statement closes the race where an interleaved
// UpdateTabTitleIfNotUserSet could observe user_titled=0 between two
// separate UPDATEs and overwrite the manual title before the lock
// flips. Returns the number of rows affected; 0 indicates the tab is
// missing.
func (s *Store) RenameTabAndLock(id int64, title string) (int64, error) {
	res, err := s.db.Exec(
		`UPDATE tab SET title = ?, user_titled = 1 WHERE id = ?`,
		title, id)
	if err != nil {
		return 0, err
	}
	return res.RowsAffected()
}

// UpdateTabTitleIfNotUserSet conditionally writes the title only when
// the tab is not user-locked. Returns rows-affected so the caller can
// distinguish "applied" (1) from "suppressed by lock or tab missing"
// (0). The lock check + write happen as one atomic UPDATE so an
// interleaved RenameTabAndLock from another goroutine cannot lose.
func (s *Store) UpdateTabTitleIfNotUserSet(id int64, title string) (int64, error) {
	res, err := s.db.Exec(
		`UPDATE tab SET title = ? WHERE id = ? AND user_titled = 0`,
		title, id)
	if err != nil {
		return 0, err
	}
	return res.RowsAffected()
}

// TouchTab marks the tab as the most recently active.
func (s *Store) TouchTab(id int64) error {
	_, err := s.db.Exec(`UPDATE tab SET last_active = ? WHERE id = ?`, time.Now().Unix(), id)
	return err
}

// DeleteTab removes a tab.
func (s *Store) DeleteTab(id int64) error {
	_, err := s.db.Exec(`DELETE FROM tab WHERE id = ?`, id)
	return err
}

var errProjectNotFound = errors.New("store: project not found")

// ErrNotFound returned when a row lookup fails.
var ErrNotFound = errProjectNotFound
