// Package core is the workspace state coordinator. It owns the Store
// and is the single entry point for the UI to read or mutate project
// and tab state. State changes fan out to subscribers via an event
// channel — Phase 2's sidebar widget will subscribe to redraw.
//
// The UI must never reach around core into the store directly. That
// boundary is what preserves the future option of moving core into a
// separate daemon process.
package core

import (
	"errors"
	"sync"

	"github.com/charliek/roost/internal/store"
)

// Re-exports so the UI doesn't import internal/store directly.
type (
	Project = store.Project
	Tab     = store.Tab
)

// Event is fired on every state change. Subscribers receive a snapshot
// of the affected entity (or its ID for deletes). The discriminator is
// the Kind field.
type Event struct {
	Kind      EventKind
	Project   *Project // populated for project events
	Tab       *Tab     // populated for tab events
	ProjectID int64    // populated for ProjectDeleted and EventNotification
	TabID     int64    // populated for TabDeleted and EventNotification
	Title     string   // populated for EventNotification
	Body      string   // populated for EventNotification
}

// EventKind is the type discriminator for Event.
type EventKind int

const (
	EventProjectAdded EventKind = iota + 1
	EventProjectRenamed
	EventProjectDeleted
	EventTabAdded
	EventTabUpdated
	EventTabDeleted
	// EventNotification fires when something (the CLI, an OSC parser,
	// future hooks) requests a notification on a tab. Subscribers are
	// expected to handle UI badging + desktop notification surface.
	EventNotification
)

// Workspace owns the persistent state and broadcasts changes.
type Workspace struct {
	store *store.Store

	mu          sync.Mutex
	subscribers []chan<- Event
}

// New wraps an opened Store.
func New(s *store.Store) *Workspace { return &Workspace{store: s} }

// Subscribe returns a channel that receives every state change. The
// channel is buffered; if it fills, events are dropped silently — no
// subscriber should block the workspace. UI subscribers must drain via
// glib.IdleAdd marshalling, since they'll receive events from the
// goroutine that performed the mutation.
func (w *Workspace) Subscribe(buffer int) <-chan Event {
	ch := make(chan Event, buffer)
	w.mu.Lock()
	w.subscribers = append(w.subscribers, ch)
	w.mu.Unlock()
	return ch
}

func (w *Workspace) emit(e Event) {
	w.mu.Lock()
	subs := append([]chan<- Event(nil), w.subscribers...)
	w.mu.Unlock()
	for _, ch := range subs {
		select {
		case ch <- e:
		default:
		}
	}
}

// LoadAll returns every project (sidebar order) with each project's tabs
// already attached. Used at app startup to rehydrate the UI.
func (w *Workspace) LoadAll() ([]ProjectWithTabs, error) {
	projects, err := w.store.ListProjects()
	if err != nil {
		return nil, err
	}
	out := make([]ProjectWithTabs, 0, len(projects))
	for _, p := range projects {
		tabs, err := w.store.ListTabs(p.ID)
		if err != nil {
			return nil, err
		}
		out = append(out, ProjectWithTabs{Project: p, Tabs: tabs})
	}
	return out, nil
}

// ProjectWithTabs is the rehydrated view of one project for the UI.
type ProjectWithTabs struct {
	Project Project
	Tabs    []Tab
}

// CreateProject inserts a project and emits an event.
func (w *Workspace) CreateProject(name, cwd string) (Project, error) {
	if name == "" {
		return Project{}, errors.New("core: project name required")
	}
	p, err := w.store.CreateProject(name, cwd)
	if err != nil {
		return Project{}, err
	}
	w.emit(Event{Kind: EventProjectAdded, Project: &p})
	return p, nil
}

// RenameProject updates the project's name and emits an event.
func (w *Workspace) RenameProject(id int64, name string) error {
	if name == "" {
		return errors.New("core: project name required")
	}
	if err := w.store.RenameProject(id, name); err != nil {
		return err
	}
	w.emit(Event{Kind: EventProjectRenamed, Project: &Project{ID: id, Name: name}})
	return nil
}

// DeleteProject removes the project + cascades to its tabs.
func (w *Workspace) DeleteProject(id int64) error {
	if err := w.store.DeleteProject(id); err != nil {
		return err
	}
	w.emit(Event{Kind: EventProjectDeleted, ProjectID: id})
	return nil
}

// CreateTab inserts a tab in the given project and emits an event.
func (w *Workspace) CreateTab(projectID int64, cwd string) (Tab, error) {
	t, err := w.store.CreateTab(projectID, cwd)
	if err != nil {
		return Tab{}, err
	}
	w.emit(Event{Kind: EventTabAdded, Tab: &t})
	return t, nil
}

// UpdateTabTitle persists a new tab title (e.g. from OSC 1/2).
func (w *Workspace) UpdateTabTitle(id int64, title string) error {
	if err := w.store.UpdateTabTitle(id, title); err != nil {
		return err
	}
	w.emit(Event{Kind: EventTabUpdated, Tab: &Tab{ID: id, Title: title}})
	return nil
}

// UpdateTabCWD persists a new cwd (e.g. from OSC 7).
func (w *Workspace) UpdateTabCWD(id int64, cwd string) error {
	if err := w.store.UpdateTabCWD(id, cwd); err != nil {
		return err
	}
	w.emit(Event{Kind: EventTabUpdated, Tab: &Tab{ID: id, CWD: cwd}})
	return nil
}

// DeleteTab removes a tab.
func (w *Workspace) DeleteTab(id int64) error {
	if err := w.store.DeleteTab(id); err != nil {
		return err
	}
	w.emit(Event{Kind: EventTabDeleted, TabID: id})
	return nil
}

// Notify emits an EventNotification for a tab. Does not persist
// anything; notifications are ephemeral state that lives in memory and
// in the UI.
func (w *Workspace) Notify(tabID int64, title, body string) error {
	if title == "" {
		return errors.New("core: notification title required")
	}
	w.emit(Event{Kind: EventNotification, TabID: tabID, Title: title, Body: body})
	return nil
}

// EnsureDefault makes sure there's at least one project containing at
// least one tab. Returns the (possibly newly created) first project + tab.
// Used at first launch.
func (w *Workspace) EnsureDefault(homeDir string) (Project, Tab, error) {
	projects, err := w.store.ListProjects()
	if err != nil {
		return Project{}, Tab{}, err
	}

	var p Project
	if len(projects) == 0 {
		p, err = w.CreateProject("default", homeDir)
		if err != nil {
			return Project{}, Tab{}, err
		}
	} else {
		p = projects[0]
	}

	tabs, err := w.store.ListTabs(p.ID)
	if err != nil {
		return Project{}, Tab{}, err
	}
	if len(tabs) > 0 {
		return p, tabs[0], nil
	}
	t, err := w.CreateTab(p.ID, p.CWD)
	if err != nil {
		return Project{}, Tab{}, err
	}
	return p, t, nil
}
