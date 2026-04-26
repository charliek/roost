package store

import (
	"path/filepath"
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
