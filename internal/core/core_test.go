package core

import (
	"path/filepath"
	"testing"

	"github.com/charliek/roost/internal/store"
)

func newWorkspace(t *testing.T) *Workspace {
	t.Helper()
	s, err := store.Open(filepath.Join(t.TempDir(), "test.db"))
	if err != nil {
		t.Fatalf("store.Open: %v", err)
	}
	t.Cleanup(func() { _ = s.Close() })
	return New(s)
}

func TestEnsureDefault(t *testing.T) {
	w := newWorkspace(t)

	p, tab, err := w.EnsureDefault("/home/charliek")
	if err != nil {
		t.Fatalf("EnsureDefault: %v", err)
	}
	if p.Name != "default" || p.CWD != "/home/charliek" {
		t.Fatalf("first project: %+v", p)
	}
	if tab.ProjectID != p.ID || tab.CWD != "/home/charliek" {
		t.Fatalf("first tab: %+v", tab)
	}

	// Second call: idempotent — same project, same tab.
	p2, tab2, err := w.EnsureDefault("/home/charliek")
	if err != nil {
		t.Fatalf("second EnsureDefault: %v", err)
	}
	if p2.ID != p.ID || tab2.ID != tab.ID {
		t.Fatalf("not idempotent: %+v %+v", p2, tab2)
	}
}

func TestEventsFireOnMutation(t *testing.T) {
	w := newWorkspace(t)
	ch := w.Subscribe(8)

	p, err := w.CreateProject("alpha", "/a")
	if err != nil {
		t.Fatalf("CreateProject: %v", err)
	}

	got := <-ch
	if got.Kind != EventProjectAdded || got.Project == nil || got.Project.Name != "alpha" {
		t.Fatalf("expected ProjectAdded, got %+v", got)
	}

	if _, err := w.CreateTab(p.ID, "/a/sub"); err != nil {
		t.Fatalf("CreateTab: %v", err)
	}
	got = <-ch
	if got.Kind != EventTabAdded || got.Tab == nil || got.Tab.CWD != "/a/sub" {
		t.Fatalf("expected TabAdded, got %+v", got)
	}
}

func TestEventChannelDoesNotBlockOnFullSubscriber(t *testing.T) {
	w := newWorkspace(t)
	_ = w.Subscribe(1) // never drained

	// Should not deadlock even though our subscriber is full.
	for i := 0; i < 5; i++ {
		if _, err := w.CreateProject("p", "/p"); err != nil {
			t.Fatalf("CreateProject %d: %v", i, err)
		}
	}
}
