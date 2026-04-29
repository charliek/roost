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
	ch := w.Subscribe(16)
	p, _ := w.CreateProject("p", "/p")
	<-ch
	tab, _ := w.CreateTab(p.ID, "/p")
	<-ch

	if err := w.Notify(tab.ID, "title", "body"); err != nil {
		t.Fatalf("Notify 1: %v", err)
	}
	// Notify emits EventNotification then EventTabNotificationChanged
	// (transitioning the has-notification flag from false to true).
	ev := <-ch
	if ev.Kind != EventNotification || ev.TabID != tab.ID || ev.Title != "title" {
		t.Fatalf("first Notify event: %+v", ev)
	}
	ev = <-ch
	if ev.Kind != EventTabNotificationChanged {
		t.Fatalf("expected EventTabNotificationChanged after Notify, got %+v", ev)
	}

	// Identical pair within window — silently dropped (no events).
	if err := w.Notify(tab.ID, "title", "body"); err != nil {
		t.Fatalf("Notify 2: %v", err)
	}
	select {
	case ev := <-ch:
		t.Fatalf("repeat Notify within cooldown emitted event: %+v", ev)
	default:
	}

	// Distinct body within the same window — fires EventNotification.
	// MarkNotification is idempotent (flag already true), so no
	// second EventTabNotificationChanged.
	if err := w.Notify(tab.ID, "title", "different body"); err != nil {
		t.Fatalf("Notify 3: %v", err)
	}
	ev = <-ch
	if ev.Kind != EventNotification || ev.Body != "different body" {
		t.Fatalf("distinct-body Notify event: %+v", ev)
	}
	select {
	case ev := <-ch:
		t.Fatalf("idempotent has-flag emitted: %+v", ev)
	default:
	}
}

func TestNotifyDedupeIsPerTab(t *testing.T) {
	w := newWorkspace(t)
	ch := w.Subscribe(16)
	p, _ := w.CreateProject("p", "/p")
	<-ch
	t1, _ := w.CreateTab(p.ID, "/p")
	<-ch
	t2, _ := w.CreateTab(p.ID, "/p")
	<-ch

	if err := w.Notify(t1.ID, "title", "body"); err != nil {
		t.Fatalf("Notify t1: %v", err)
	}
	<-ch // EventNotification
	<-ch // EventTabNotificationChanged
	// Same content on a different tab still fires — dedupe is per-tab.
	if err := w.Notify(t2.ID, "title", "body"); err != nil {
		t.Fatalf("Notify t2: %v", err)
	}
	ev := <-ch
	if ev.Kind != EventNotification || ev.TabID != t2.ID {
		t.Fatalf("expected EventNotification for t2, got %+v", ev)
	}
}

func TestSetTabAgentStateEmitsOnTransitionsOnly(t *testing.T) {
	w := newWorkspace(t)
	ch := w.Subscribe(8)
	p, _ := w.CreateProject("p", "/p")
	<-ch
	tab, _ := w.CreateTab(p.ID, "/p")
	<-ch

	w.SetTabAgentState(tab.ID, TabAgentRunning)
	ev := <-ch
	if ev.Kind != EventTabStateChanged || ev.TabID != tab.ID || ev.AgentState != TabAgentRunning {
		t.Fatalf("first SetTabAgentState event: %+v", ev)
	}
	if got := w.TabAgentState(tab.ID); got != TabAgentRunning {
		t.Fatalf("TabAgentState: got %q want %q", got, TabAgentRunning)
	}

	// Idempotent re-set: silent.
	w.SetTabAgentState(tab.ID, TabAgentRunning)
	select {
	case ev := <-ch:
		t.Fatalf("idempotent re-set emitted: %+v", ev)
	default:
	}

	// Real transition: fires.
	w.SetTabAgentState(tab.ID, TabAgentNeedsInput)
	ev = <-ch
	if ev.AgentState != TabAgentNeedsInput {
		t.Fatalf("transition event: %+v", ev)
	}

	// None clears the map entry; lookup returns TabAgentNone.
	w.SetTabAgentState(tab.ID, TabAgentNone)
	<-ch
	if got := w.TabAgentState(tab.ID); got != TabAgentNone {
		t.Fatalf("after clear: got %q", got)
	}
}

func TestHookSessionFlagIsSilent(t *testing.T) {
	w := newWorkspace(t)
	ch := w.Subscribe(8)
	p, _ := w.CreateProject("p", "/p")
	<-ch
	tab, _ := w.CreateTab(p.ID, "/p")
	<-ch

	if w.IsHookSessionActive(tab.ID) {
		t.Fatal("expected no hook session initially")
	}
	w.SetHookSessionActive(tab.ID, true)
	if !w.IsHookSessionActive(tab.ID) {
		t.Fatal("expected hook session active after set")
	}
	w.SetHookSessionActive(tab.ID, false)
	if w.IsHookSessionActive(tab.ID) {
		t.Fatal("expected hook session inactive after clear")
	}
	// No events for any of the above.
	select {
	case ev := <-ch:
		t.Fatalf("hook flag emitted event: %+v", ev)
	default:
	}
}

func TestMarkNotificationFiresOnTransitionsAndNotifySetsFlag(t *testing.T) {
	w := newWorkspace(t)
	ch := w.Subscribe(16)
	p, _ := w.CreateProject("p", "/p")
	<-ch
	tab, _ := w.CreateTab(p.ID, "/p")
	<-ch

	if err := w.Notify(tab.ID, "title", "body"); err != nil {
		t.Fatalf("Notify: %v", err)
	}
	// Notify emits EventNotification followed by EventTabNotificationChanged.
	ev := <-ch
	if ev.Kind != EventNotification {
		t.Fatalf("expected EventNotification first, got %+v", ev)
	}
	ev = <-ch
	if ev.Kind != EventTabNotificationChanged || ev.TabID != tab.ID {
		t.Fatalf("expected EventTabNotificationChanged, got %+v", ev)
	}
	if !w.HasNotification(tab.ID) {
		t.Fatal("expected HasNotification true after Notify")
	}

	// Idempotent set: no event.
	w.MarkNotification(tab.ID, true)
	select {
	case ev := <-ch:
		t.Fatalf("idempotent MarkNotification emitted: %+v", ev)
	default:
	}

	// Clear: fires once.
	w.MarkNotification(tab.ID, false)
	ev = <-ch
	if ev.Kind != EventTabNotificationChanged {
		t.Fatalf("clear event: %+v", ev)
	}
	if w.HasNotification(tab.ID) {
		t.Fatal("expected HasNotification false after clear")
	}
}

func TestDeleteTabClearsAgentStateAndPopulatesProjectID(t *testing.T) {
	w := newWorkspace(t)
	ch := w.Subscribe(8)
	p, _ := w.CreateProject("p", "/p")
	<-ch
	tab, _ := w.CreateTab(p.ID, "/p")
	<-ch

	w.SetTabAgentState(tab.ID, TabAgentRunning)
	<-ch
	w.SetHookSessionActive(tab.ID, true)
	w.MarkNotification(tab.ID, true)
	<-ch

	if err := w.DeleteTab(tab.ID); err != nil {
		t.Fatalf("DeleteTab: %v", err)
	}
	ev := <-ch
	if ev.Kind != EventTabDeleted || ev.TabID != tab.ID || ev.ProjectID != p.ID {
		t.Fatalf("EventTabDeleted: %+v", ev)
	}
	if w.TabAgentState(tab.ID) != TabAgentNone {
		t.Fatal("agent state not cleared")
	}
	if w.IsHookSessionActive(tab.ID) {
		t.Fatal("hook flag not cleared")
	}
	if w.HasNotification(tab.ID) {
		t.Fatal("notification flag not cleared")
	}
}

func TestValidTabAgentState(t *testing.T) {
	for _, s := range []string{"none", "running", "needs_input", "idle"} {
		if !ValidTabAgentState(s) {
			t.Errorf("ValidTabAgentState(%q) = false, want true", s)
		}
	}
	for _, s := range []string{"", "Running", "NEEDS_INPUT", "done", "garbage"} {
		if ValidTabAgentState(s) {
			t.Errorf("ValidTabAgentState(%q) = true, want false", s)
		}
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
