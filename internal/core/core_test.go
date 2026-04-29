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

func TestRenameTabLocksAgainstOSC(t *testing.T) {
	w := newWorkspace(t)
	ch := w.Subscribe(8)
	p, _ := w.CreateProject("p", "/p")
	<-ch // EventProjectAdded
	tab, _ := w.CreateTab(p.ID, "/p")
	<-ch // EventTabAdded

	if err := w.RenameTab(tab.ID, "manual"); err != nil {
		t.Fatalf("RenameTab: %v", err)
	}
	ev := <-ch
	if ev.Kind != EventTabUpdated || ev.Tab == nil ||
		ev.Tab.Title != "manual" || !ev.Tab.UserTitled {
		t.Fatalf("RenameTab event: %+v %+v", ev, ev.Tab)
	}

	// OSC after lock: silent no-op, no event.
	if err := w.SetTabTitleFromOSC(tab.ID, "from-osc"); err != nil {
		t.Fatalf("SetTabTitleFromOSC: %v", err)
	}
	select {
	case ev := <-ch:
		t.Fatalf("OSC after lock emitted event: %+v", ev)
	default:
	}

	tabs, _ := w.LoadAll()
	if tabs[0].Tabs[0].Title != "manual" || !tabs[0].Tabs[0].UserTitled {
		t.Fatalf("OSC clobbered locked tab: %+v", tabs[0].Tabs[0])
	}
}

func TestSetTabTitleFromOSCBeforeRenameApplies(t *testing.T) {
	w := newWorkspace(t)
	ch := w.Subscribe(8)
	p, _ := w.CreateProject("p", "/p")
	<-ch
	tab, _ := w.CreateTab(p.ID, "/p")
	<-ch

	if err := w.SetTabTitleFromOSC(tab.ID, "from-osc"); err != nil {
		t.Fatalf("SetTabTitleFromOSC: %v", err)
	}
	ev := <-ch
	if ev.Kind != EventTabUpdated || ev.Tab.Title != "from-osc" {
		t.Fatalf("OSC event: %+v", ev)
	}
	if ev.Tab.UserTitled {
		t.Errorf("OSC event populated UserTitled=true unexpectedly")
	}

	if err := w.RenameTab(tab.ID, "manual"); err != nil {
		t.Fatalf("RenameTab: %v", err)
	}
	<-ch // EventTabUpdated for rename

	if err := w.SetTabTitleFromOSC(tab.ID, "post-rename"); err != nil {
		t.Fatalf("SetTabTitleFromOSC: %v", err)
	}
	select {
	case ev := <-ch:
		t.Fatalf("OSC after rename emitted event: %+v", ev)
	default:
	}
}

func TestSetTabTitleFromOSCEmptyIsNoOp(t *testing.T) {
	w := newWorkspace(t)
	ch := w.Subscribe(8)
	p, _ := w.CreateProject("p", "/p")
	<-ch
	tab, _ := w.CreateTab(p.ID, "/p")
	<-ch

	if err := w.SetTabTitleFromOSC(tab.ID, ""); err != nil {
		t.Fatalf("SetTabTitleFromOSC empty: %v", err)
	}
	select {
	case ev := <-ch:
		t.Fatalf("empty OSC emitted event: %+v", ev)
	default:
	}
}

func TestRenameTabRequiresTitle(t *testing.T) {
	w := newWorkspace(t)
	if err := w.RenameTab(1, ""); err == nil {
		t.Fatalf("RenameTab with empty title: want error, got nil")
	}
}

func TestNotifyDedupesExactRepeatsWithinCooldown(t *testing.T) {
	w := newWorkspace(t)
	ch := w.Subscribe(8)
	p, _ := w.CreateProject("p", "/p")
	<-ch
	tab, _ := w.CreateTab(p.ID, "/p")
	<-ch

	if err := w.Notify(tab.ID, "title", "body"); err != nil {
		t.Fatalf("Notify 1: %v", err)
	}
	ev := <-ch
	if ev.Kind != EventNotification || ev.TabID != tab.ID || ev.Title != "title" {
		t.Fatalf("first Notify event: %+v", ev)
	}

	// Identical pair within window — silently dropped.
	if err := w.Notify(tab.ID, "title", "body"); err != nil {
		t.Fatalf("Notify 2: %v", err)
	}
	select {
	case ev := <-ch:
		t.Fatalf("repeat Notify within cooldown emitted event: %+v", ev)
	default:
	}

	// Distinct body within the same window — fires.
	if err := w.Notify(tab.ID, "title", "different body"); err != nil {
		t.Fatalf("Notify 3: %v", err)
	}
	ev = <-ch
	if ev.Body != "different body" {
		t.Fatalf("distinct-body Notify dropped or wrong: %+v", ev)
	}
}

func TestNotifyDedupeIsPerTab(t *testing.T) {
	w := newWorkspace(t)
	ch := w.Subscribe(8)
	p, _ := w.CreateProject("p", "/p")
	<-ch
	t1, _ := w.CreateTab(p.ID, "/p")
	<-ch
	t2, _ := w.CreateTab(p.ID, "/p")
	<-ch

	if err := w.Notify(t1.ID, "title", "body"); err != nil {
		t.Fatalf("Notify t1: %v", err)
	}
	<-ch
	// Same content on a different tab still fires — dedupe is per-tab.
	if err := w.Notify(t2.ID, "title", "body"); err != nil {
		t.Fatalf("Notify t2: %v", err)
	}
	ev := <-ch
	if ev.TabID != t2.ID {
		t.Fatalf("expected event for t2, got %+v", ev)
	}
}

func TestNotifyRequiresTitle(t *testing.T) {
	w := newWorkspace(t)
	if err := w.Notify(1, "", "body"); err == nil {
		t.Fatalf("Notify with empty title: want error, got nil")
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
