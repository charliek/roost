package store

import (
	"database/sql"
	"path/filepath"
	"sync"
	"testing"
)

func openTemp(t *testing.T) *Store {
	t.Helper()
	s, err := Open(filepath.Join(t.TempDir(), "test.db"))
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	t.Cleanup(func() { _ = s.Close() })
	return s
}

func TestProjectCRUD(t *testing.T) {
	s := openTemp(t)

	p1, err := s.CreateProject("alpha", "/tmp/a")
	if err != nil {
		t.Fatalf("CreateProject: %v", err)
	}
	if p1.Position != 0 {
		t.Errorf("first project position: got %d want 0", p1.Position)
	}

	p2, err := s.CreateProject("beta", "/tmp/b")
	if err != nil {
		t.Fatalf("CreateProject: %v", err)
	}
	if p2.Position != 1 {
		t.Errorf("second project position: got %d want 1", p2.Position)
	}

	projects, err := s.ListProjects()
	if err != nil {
		t.Fatalf("ListProjects: %v", err)
	}
	if len(projects) != 2 || projects[0].ID != p1.ID || projects[1].ID != p2.ID {
		t.Fatalf("ListProjects: got %+v", projects)
	}

	if err := s.RenameProject(p1.ID, "alpha-renamed"); err != nil {
		t.Fatalf("RenameProject: %v", err)
	}
	projects, _ = s.ListProjects()
	if projects[0].Name != "alpha-renamed" {
		t.Errorf("rename: got %q", projects[0].Name)
	}
}

func TestTabCRUDAndCascade(t *testing.T) {
	s := openTemp(t)

	p, _ := s.CreateProject("p", "/p")

	t1, err := s.CreateTab(p.ID, "/p/sub")
	if err != nil {
		t.Fatalf("CreateTab: %v", err)
	}
	t2, _ := s.CreateTab(p.ID, "/p")
	if t1.Position != 0 || t2.Position != 1 {
		t.Errorf("tab positions: got %d,%d want 0,1", t1.Position, t2.Position)
	}

	if err := s.UpdateTabTitle(t1.ID, "hello"); err != nil {
		t.Fatalf("UpdateTabTitle: %v", err)
	}
	if err := s.UpdateTabCWD(t1.ID, "/p/new"); err != nil {
		t.Fatalf("UpdateTabCWD: %v", err)
	}

	tabs, err := s.ListTabs(p.ID)
	if err != nil {
		t.Fatalf("ListTabs: %v", err)
	}
	if len(tabs) != 2 || tabs[0].Title != "hello" || tabs[0].CWD != "/p/new" {
		t.Fatalf("ListTabs: %+v", tabs)
	}

	if err := s.DeleteProject(p.ID); err != nil {
		t.Fatalf("DeleteProject: %v", err)
	}
	tabs, _ = s.ListTabs(p.ID)
	if len(tabs) != 0 {
		t.Errorf("expected tabs cascaded away, got %d", len(tabs))
	}
}

func TestUserTitledLockBlocksOSC(t *testing.T) {
	s := openTemp(t)
	p, _ := s.CreateProject("p", "/p")
	tab, _ := s.CreateTab(p.ID, "/p")

	n, err := s.RenameTabAndLock(tab.ID, "manual")
	if err != nil {
		t.Fatalf("RenameTabAndLock: %v", err)
	}
	if n != 1 {
		t.Errorf("RenameTabAndLock rows-affected: got %d want 1", n)
	}

	tabs, _ := s.ListTabs(p.ID)
	if len(tabs) != 1 || !tabs[0].UserTitled || tabs[0].Title != "manual" {
		t.Fatalf("after lock: %+v", tabs)
	}

	n, err = s.UpdateTabTitleIfNotUserSet(tab.ID, "from-osc")
	if err != nil {
		t.Fatalf("UpdateTabTitleIfNotUserSet: %v", err)
	}
	if n != 0 {
		t.Errorf("locked OSC write rows-affected: got %d want 0", n)
	}
	tabs, _ = s.ListTabs(p.ID)
	if tabs[0].Title != "manual" {
		t.Errorf("title clobbered by OSC: got %q want %q", tabs[0].Title, "manual")
	}
}

func TestUnlockedTabAcceptsOSC(t *testing.T) {
	s := openTemp(t)
	p, _ := s.CreateProject("p", "/p")
	tab, _ := s.CreateTab(p.ID, "/p")

	n, err := s.UpdateTabTitleIfNotUserSet(tab.ID, "from-osc")
	if err != nil {
		t.Fatalf("UpdateTabTitleIfNotUserSet: %v", err)
	}
	if n != 1 {
		t.Errorf("unlocked OSC write rows-affected: got %d want 1", n)
	}
	tabs, _ := s.ListTabs(p.ID)
	if tabs[0].Title != "from-osc" || tabs[0].UserTitled {
		t.Fatalf("unlocked write didn't take: %+v", tabs)
	}
}

// TestUserTitledLockRace verifies the lock-wins invariant under
// concurrent RenameTabAndLock (user) and UpdateTabTitleIfNotUserSet
// (OSC). The atomicity guarantee under test: once any RenameTabAndLock
// has run, the only title values the tab can ever hold are values
// passed to RenameTabAndLock — never an OSC string.
//
// Both goroutines forward errors to a shared channel so a transient
// driver failure can't masquerade as the test passing.
func TestUserTitledLockRace(t *testing.T) {
	s := openTemp(t)
	p, _ := s.CreateProject("p", "/p")
	tab, _ := s.CreateTab(p.ID, "/p")
	if err := s.UpdateTabTitle(tab.ID, "initial"); err != nil {
		t.Fatalf("UpdateTabTitle: %v", err)
	}

	const N = 32
	const userTitle = "manual"
	var wg sync.WaitGroup
	errCh := make(chan error, 2*N)
	wg.Add(2 * N)
	for i := 0; i < N; i++ {
		go func() {
			defer wg.Done()
			if _, err := s.RenameTabAndLock(tab.ID, userTitle); err != nil {
				errCh <- err
			}
		}()
		go func() {
			defer wg.Done()
			if _, err := s.UpdateTabTitleIfNotUserSet(tab.ID, "osc"); err != nil {
				errCh <- err
			}
		}()
	}
	wg.Wait()
	close(errCh)
	for err := range errCh {
		t.Errorf("concurrent write error: %v", err)
	}

	tabs, _ := s.ListTabs(p.ID)
	if len(tabs) != 1 || !tabs[0].UserTitled {
		t.Fatalf("expected lock set after race: %+v", tabs)
	}
	// The only titles ever written by the user goroutine are
	// userTitle; the only ones written by the OSC goroutine are "osc"
	// (and only when the lock wasn't yet set). The atomicity bug
	// CodeRabbit flagged would manifest as title="osc" after at least
	// one RenameTabAndLock has run — which the post-race state below
	// would catch because every RenameTabAndLock has run by now.
	if tabs[0].Title != userTitle {
		t.Errorf("atomicity violated: title=%q want %q (an OSC write clobbered the manual rename)",
			tabs[0].Title, userTitle)
	}

	// One more OSC write post-lock must be a no-op.
	n, err := s.UpdateTabTitleIfNotUserSet(tab.ID, "post-lock")
	if err != nil {
		t.Fatalf("UpdateTabTitleIfNotUserSet: %v", err)
	}
	if n != 0 {
		t.Errorf("post-lock OSC rows-affected: got %d want 0", n)
	}
	final, _ := s.ListTabs(p.ID)
	if final[0].Title == "post-lock" {
		t.Errorf("post-lock OSC took effect: %q", final[0].Title)
	}
}

// TestUserTitledMigrationApplied opens a database created with only
// the 0001 schema (simulated by stripping the user_titled column),
// then re-runs Open and verifies 0002 brings the column back with
// default 0 and the existing row intact.
func TestUserTitledMigrationApplied(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "test.db")

	// First: open + insert with the current code (which already runs
	// 0002), then drop the column to simulate a 0001-only database.
	s, err := Open(path)
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	p, _ := s.CreateProject("p", "/p")
	tab, _ := s.CreateTab(p.ID, "/p")
	if err := s.UpdateTabTitle(tab.ID, "preserved"); err != nil {
		t.Fatalf("UpdateTabTitle: %v", err)
	}

	// Forcibly roll back to a 0001-only state: drop the column and the
	// migration record. SQLite's DROP COLUMN was added in 3.35; modernc
	// is current enough.
	if _, err := s.db.Exec(`ALTER TABLE tab DROP COLUMN user_titled`); err != nil {
		t.Fatalf("DROP COLUMN: %v", err)
	}
	if _, err := s.db.Exec(`DELETE FROM schema_migrations WHERE version = 2`); err != nil {
		t.Fatalf("DELETE migration: %v", err)
	}
	if err := s.Close(); err != nil {
		t.Fatalf("Close: %v", err)
	}

	// Re-open: 0002 should re-apply.
	s, err = Open(path)
	if err != nil {
		t.Fatalf("re-Open: %v", err)
	}
	t.Cleanup(func() { _ = s.Close() })

	tabs, err := s.ListTabs(p.ID)
	if err != nil {
		t.Fatalf("ListTabs after migrate: %v", err)
	}
	if len(tabs) != 1 || tabs[0].Title != "preserved" || tabs[0].UserTitled {
		t.Fatalf("migration didn't preserve row or default user_titled: %+v", tabs)
	}

	var hasCol int
	if err := s.db.QueryRow(
		`SELECT COUNT(*) FROM pragma_table_info('tab') WHERE name = 'user_titled'`,
	).Scan(&hasCol); err != nil && err != sql.ErrNoRows {
		t.Fatalf("pragma table_info: %v", err)
	}
	if hasCol != 1 {
		t.Errorf("user_titled column missing after re-migrate")
	}
}

func TestMigrateIdempotent(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "test.db")

	s, err := Open(path)
	if err != nil {
		t.Fatalf("first open: %v", err)
	}
	if _, err := s.CreateProject("p", "/p"); err != nil {
		t.Fatalf("CreateProject: %v", err)
	}
	_ = s.Close()

	// Reopen — migrations must not re-apply.
	s, err = Open(path)
	if err != nil {
		t.Fatalf("reopen: %v", err)
	}
	t.Cleanup(func() { _ = s.Close() })

	projects, _ := s.ListProjects()
	if len(projects) != 1 {
		t.Fatalf("data lost across reopen: %+v", projects)
	}
}
