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

// mustProject and mustTab are setup helpers that fail-fast on store
// errors so tests don't panic when indexing the returned slices.
func mustProject(t *testing.T, s *Store, name, cwd string) Project {
	t.Helper()
	p, err := s.CreateProject(name, cwd)
	if err != nil {
		t.Fatalf("CreateProject(%q,%q): %v", name, cwd, err)
	}
	return p
}

func mustTab(t *testing.T, s *Store, projectID int64, cwd string) Tab {
	t.Helper()
	tab, err := s.CreateTab(projectID, cwd)
	if err != nil {
		t.Fatalf("CreateTab(%d,%q): %v", projectID, cwd, err)
	}
	return tab
}

// mustListTabs reads tabs and asserts a minimum length so callers can
// index into the result safely. Use len-aware assertions on the
// returned slice; this just guards against the panic-on-empty case.
func mustListTabs(t *testing.T, s *Store, projectID int64, minLen int) []Tab {
	t.Helper()
	tabs, err := s.ListTabs(projectID)
	if err != nil {
		t.Fatalf("ListTabs(%d): %v", projectID, err)
	}
	if len(tabs) < minLen {
		t.Fatalf("ListTabs(%d): got %d tabs, want >= %d (%+v)", projectID, len(tabs), minLen, tabs)
	}
	return tabs
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
	projects, err = s.ListProjects()
	if err != nil {
		t.Fatalf("ListProjects after rename: %v", err)
	}
	if len(projects) == 0 || projects[0].Name != "alpha-renamed" {
		t.Errorf("rename: got %+v", projects)
	}
}

func TestTabCRUDAndCascade(t *testing.T) {
	s := openTemp(t)

	p := mustProject(t, s, "p", "/p")
	t1 := mustTab(t, s, p.ID, "/p/sub")
	t2 := mustTab(t, s, p.ID, "/p")
	if t1.Position != 0 || t2.Position != 1 {
		t.Errorf("tab positions: got %d,%d want 0,1", t1.Position, t2.Position)
	}

	if err := s.UpdateTabTitle(t1.ID, "hello"); err != nil {
		t.Fatalf("UpdateTabTitle: %v", err)
	}
	if err := s.UpdateTabCWD(t1.ID, "/p/new"); err != nil {
		t.Fatalf("UpdateTabCWD: %v", err)
	}

	tabs := mustListTabs(t, s, p.ID, 2)
	if tabs[0].Title != "hello" || tabs[0].CWD != "/p/new" {
		t.Fatalf("ListTabs: %+v", tabs)
	}

	if err := s.DeleteProject(p.ID); err != nil {
		t.Fatalf("DeleteProject: %v", err)
	}
	tabs, err := s.ListTabs(p.ID)
	if err != nil {
		t.Fatalf("ListTabs after delete: %v", err)
	}
	if len(tabs) != 0 {
		t.Errorf("expected tabs cascaded away, got %d", len(tabs))
	}
}

func TestUserTitledLockBlocksOSC(t *testing.T) {
	s := openTemp(t)
	p := mustProject(t, s, "p", "/p")
	tab := mustTab(t, s, p.ID, "/p")

	n, err := s.RenameTabAndLock(tab.ID, "manual")
	if err != nil {
		t.Fatalf("RenameTabAndLock: %v", err)
	}
	if n != 1 {
		t.Errorf("RenameTabAndLock rows-affected: got %d want 1", n)
	}

	tabs := mustListTabs(t, s, p.ID, 1)
	if !tabs[0].UserTitled || tabs[0].Title != "manual" {
		t.Fatalf("after lock: %+v", tabs)
	}

	n, err = s.UpdateTabTitleIfNotUserSet(tab.ID, "from-osc")
	if err != nil {
		t.Fatalf("UpdateTabTitleIfNotUserSet: %v", err)
	}
	if n != 0 {
		t.Errorf("locked OSC write rows-affected: got %d want 0", n)
	}
	tabs = mustListTabs(t, s, p.ID, 1)
	if tabs[0].Title != "manual" {
		t.Errorf("title clobbered by OSC: got %q want %q", tabs[0].Title, "manual")
	}
}

func TestUnlockedTabAcceptsOSC(t *testing.T) {
	s := openTemp(t)
	p := mustProject(t, s, "p", "/p")
	tab := mustTab(t, s, p.ID, "/p")

	n, err := s.UpdateTabTitleIfNotUserSet(tab.ID, "from-osc")
	if err != nil {
		t.Fatalf("UpdateTabTitleIfNotUserSet: %v", err)
	}
	if n != 1 {
		t.Errorf("unlocked OSC write rows-affected: got %d want 1", n)
	}
	tabs := mustListTabs(t, s, p.ID, 1)
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
	p := mustProject(t, s, "p", "/p")
	tab := mustTab(t, s, p.ID, "/p")
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

	tabs := mustListTabs(t, s, p.ID, 1)
	if !tabs[0].UserTitled {
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
	final := mustListTabs(t, s, p.ID, 1)
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
	p := mustProject(t, s, "p", "/p")
	tab := mustTab(t, s, p.ID, "/p")
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

func TestReorderProjects(t *testing.T) {
	s := openTemp(t)

	a := mustProject(t, s, "a", "/a")
	b := mustProject(t, s, "b", "/b")
	c := mustProject(t, s, "c", "/c")

	// Move c to the front, then b, then a.
	if err := s.ReorderProjects([]int64{c.ID, b.ID, a.ID}); err != nil {
		t.Fatalf("ReorderProjects: %v", err)
	}
	projects, err := s.ListProjects()
	if err != nil {
		t.Fatalf("ListProjects: %v", err)
	}
	if len(projects) != 3 ||
		projects[0].ID != c.ID || projects[0].Position != 0 ||
		projects[1].ID != b.ID || projects[1].Position != 1 ||
		projects[2].ID != a.ID || projects[2].Position != 2 {
		t.Fatalf("after reorder: %+v", projects)
	}

	// Mismatched length is rejected and leaves order untouched.
	if err := s.ReorderProjects([]int64{c.ID, b.ID}); err == nil {
		t.Errorf("ReorderProjects: expected error on short list, got nil")
	}
	// Unknown id is rejected.
	if err := s.ReorderProjects([]int64{c.ID, b.ID, 99999}); err == nil {
		t.Errorf("ReorderProjects: expected error on unknown id, got nil")
	}
	// Duplicate id is rejected.
	if err := s.ReorderProjects([]int64{c.ID, c.ID, a.ID}); err == nil {
		t.Errorf("ReorderProjects: expected error on duplicate id, got nil")
	}

	// Order should still be c,b,a after the rejected calls.
	projects, err = s.ListProjects()
	if err != nil {
		t.Fatalf("ListProjects after rejected reorders: %v", err)
	}
	if len(projects) != 3 || projects[0].ID != c.ID || projects[2].ID != a.ID {
		t.Fatalf("rejected reorder mutated state: %+v", projects)
	}
}

func TestReorderTabs(t *testing.T) {
	s := openTemp(t)

	p := mustProject(t, s, "p", "/p")
	other := mustProject(t, s, "other", "/other")
	t1 := mustTab(t, s, p.ID, "/p/1")
	t2 := mustTab(t, s, p.ID, "/p/2")
	t3 := mustTab(t, s, p.ID, "/p/3")
	otherTab := mustTab(t, s, other.ID, "/other/1")

	// Reverse the order in p.
	if err := s.ReorderTabs(p.ID, []int64{t3.ID, t2.ID, t1.ID}); err != nil {
		t.Fatalf("ReorderTabs: %v", err)
	}
	tabs := mustListTabs(t, s, p.ID, 3)
	if tabs[0].ID != t3.ID || tabs[0].Position != 0 ||
		tabs[1].ID != t2.ID || tabs[1].Position != 1 ||
		tabs[2].ID != t1.ID || tabs[2].Position != 2 {
		t.Fatalf("after reorder: %+v", tabs)
	}

	// A tab from another project must be rejected.
	if err := s.ReorderTabs(p.ID, []int64{t3.ID, t2.ID, otherTab.ID}); err == nil {
		t.Errorf("ReorderTabs: expected error when slice contains another project's tab")
	}
	// Other project's tab order must be untouched.
	otherTabs := mustListTabs(t, s, other.ID, 1)
	if otherTabs[0].ID != otherTab.ID || otherTabs[0].Position != 0 {
		t.Fatalf("other project's tabs disturbed: %+v", otherTabs)
	}

	// Order still reversed in p.
	tabs = mustListTabs(t, s, p.ID, 3)
	if tabs[0].ID != t3.ID || tabs[2].ID != t1.ID {
		t.Fatalf("rejected reorder mutated state: %+v", tabs)
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

	projects, err := s.ListProjects()
	if err != nil {
		t.Fatalf("ListProjects after reopen: %v", err)
	}
	if len(projects) != 1 {
		t.Fatalf("data lost across reopen: %+v", projects)
	}
}
