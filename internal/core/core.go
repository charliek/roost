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
	"time"

	"github.com/charliek/roost/internal/store"
)

// notifyCooldown is the per-tab dedupe window for identical (title, body)
// pairs. Short enough to be invisible to humans, long enough to absorb
// double-fires from misbehaving senders or rapid OSC-burst loops that
// slip past the OSC 9 ConEmu filter.
const notifyCooldown = 1 * time.Second

type notifyDedupe struct {
	at  time.Time
	key string
}

// Re-exports so the UI doesn't import internal/store directly.
type (
	Project = store.Project
	Tab     = store.Tab
)

// Event is fired on every state change. Subscribers receive a snapshot
// of the affected entity (or its ID for deletes). The discriminator is
// the Kind field.
//
// Tab payload contract for EventTabUpdated: only the ID and the field
// that changed are populated. RenameTab emits a Tab with ID+Title and
// UserTitled=true; SetTabTitleFromOSC emits ID+Title (UserTitled stays
// false because the OSC path never writes through a locked tab);
// UpdateTabCWD emits ID+CWD. Subscribers must guard on the specific
// field rather than treating the Tab as a complete snapshot. The
// zero-value semantics mean a future *unlock* event cannot be expressed
// as a bool flip — if unlock UX lands, this contract grows to a
// distinct event kind or a tri-state.
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
	// lastNotify holds the last (title|body) and timestamp per tab,
	// used by Notify to drop exact-repeat notifications inside
	// notifyCooldown. Guarded by mu.
	lastNotify map[int64]notifyDedupe
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

// RenameTab persists a user-chosen title and locks the tab against
// future OSC 1/2 overwrites. Used by the Cmd-R popover and the IPC
// tab.set_title method (the CLI path).
//
// The title write and lock flip happen as one atomic UPDATE in the
// store, so an interleaved SetTabTitleFromOSC from the PTY drain
// cannot observe user_titled=0 between two separate writes and clobber
// the manual title before the lock takes effect.
func (w *Workspace) RenameTab(id int64, title string) error {
	if title == "" {
		return errors.New("core: RenameTab requires non-empty title")
	}
	n, err := w.store.RenameTabAndLock(id, title)
	if err != nil {
		return err
	}
	if n == 0 {
		return nil // tab missing — caller is racing with DeleteTab
	}
	w.emit(Event{Kind: EventTabUpdated, Tab: &Tab{ID: id, Title: title, UserTitled: true}})
	return nil
}

// SetTabTitleFromOSC persists a shell-emitted title (OSC 1/2) only if
// the tab is not user-locked. The conditional UPDATE makes the lock
// check atomic with the write, so an interleaved RenameTab cannot
// lose. Returns nil with no event when the write is suppressed (lock
// or tab missing) — the UI distinguishes by gating on the in-memory
// UserTitled flag before calling.
//
// Defensive empty-string guard: callers handle empty OSC titles in
// their own UI fallback path, so we should never see one here.
func (w *Workspace) SetTabTitleFromOSC(id int64, title string) error {
	if title == "" {
		return nil
	}
	n, err := w.store.UpdateTabTitleIfNotUserSet(id, title)
	if err != nil {
		return err
	}
	if n == 0 {
		return nil
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
//
// Identical (title, body) pairs on the same tab inside notifyCooldown
// are dropped silently. Distinct content within the window still fires;
// only exact repeats are suppressed. The cooldown protects against
// scripts that double-fire and against pathological OSC streams that
// slip past the scanner's ConEmu filter.
func (w *Workspace) Notify(tabID int64, title, body string) error {
	if title == "" {
		return errors.New("core: notification title required")
	}
	key := title + "\x00" + body
	now := time.Now()
	w.mu.Lock()
	if w.lastNotify == nil {
		w.lastNotify = make(map[int64]notifyDedupe)
	}
	if prev, ok := w.lastNotify[tabID]; ok && prev.key == key && now.Sub(prev.at) < notifyCooldown {
		w.mu.Unlock()
		return nil
	}
	w.lastNotify[tabID] = notifyDedupe{at: now, key: key}
	w.mu.Unlock()
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
