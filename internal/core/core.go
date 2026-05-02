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
	"database/sql"
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

// TabAgentState is the sticky per-tab agent status. Independent of
// "has pending notification" — state survives focus events; notification
// flag clears on focus.
//
// Wire format is the constant string (not the int), so external scripts
// invoking tab.set_state stay stable across refactors.
type TabAgentState string

const (
	TabAgentNone       TabAgentState = "none"
	TabAgentRunning    TabAgentState = "running"
	TabAgentNeedsInput TabAgentState = "needs_input"
	TabAgentIdle       TabAgentState = "idle"
)

// ValidTabAgentState returns true iff s matches one of the four
// canonical state strings. Callers (the IPC server) reject everything
// else as bad_request.
func ValidTabAgentState(s string) bool {
	switch TabAgentState(s) {
	case TabAgentNone, TabAgentRunning, TabAgentNeedsInput, TabAgentIdle:
		return true
	}
	return false
}

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
	Kind              EventKind
	Project           *Project      // populated for project events
	Tab               *Tab          // populated for tab events
	ProjectID         int64         // populated for ProjectDeleted, EventTabDeleted, EventTabsReordered
	TabID             int64         // populated for TabDeleted, EventNotification, EventTabStateChanged, EventTabNotificationChanged
	Title             string        // populated for EventNotification
	Body              string        // populated for EventNotification
	AgentState        TabAgentState // populated for EventTabStateChanged
	OrderedProjectIDs []int64       // populated for EventProjectsReordered
	OrderedTabIDs     []int64       // populated for EventTabsReordered
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
	// EventTabStateChanged fires when a tab's TabAgentState transitions.
	// Idempotent re-sets are silent.
	EventTabStateChanged
	// EventTabNotificationChanged fires when the per-tab "has pending
	// notification" flag flips. Used by the UI to redraw indicators
	// independently of EventNotification (which only fires on new
	// notifications, not on the user clearing them by focusing the tab).
	EventTabNotificationChanged
	// EventProjectsReordered fires after the sidebar order is persisted.
	// OrderedProjectIDs holds the new order (positions 0..N-1).
	EventProjectsReordered
	// EventTabsReordered fires after a project's tab order is persisted.
	// ProjectID identifies the project; OrderedTabIDs holds the new order.
	EventTabsReordered
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
	// agentState, hookActive, hasNotification are the per-tab agent
	// surface. In-memory only — Roost restart resets them; agents
	// re-emit SessionStart, so this avoids tracking liveness against
	// possibly-dead processes. Guarded by mu.
	agentState      map[int64]TabAgentState
	hookActive      map[int64]bool
	hasNotification map[int64]bool
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

// DeleteProject removes the project + cascades to its tabs. Clears
// in-memory per-tab state for the cascaded tabs too — without this,
// agentState/hookActive/hasNotification entries for the deleted tabs
// would linger until next process restart.
func (w *Workspace) DeleteProject(id int64) error {
	tabs, err := w.store.ListTabs(id)
	if err != nil {
		return err
	}
	if err := w.store.DeleteProject(id); err != nil {
		return err
	}
	for _, t := range tabs {
		w.clearTabState(t.ID)
	}
	w.emit(Event{Kind: EventProjectDeleted, ProjectID: id})
	return nil
}

// ReorderProjects persists a new sidebar order and emits an event. The
// caller (typically the UI after a drag) is responsible for sending the
// full set of project IDs in their desired visual order.
func (w *Workspace) ReorderProjects(orderedIDs []int64) error {
	if err := w.store.ReorderProjects(orderedIDs); err != nil {
		return err
	}
	// Copy so callers mutating their slice can't affect subscribers.
	ids := append([]int64(nil), orderedIDs...)
	w.emit(Event{Kind: EventProjectsReordered, OrderedProjectIDs: ids})
	return nil
}

// ReorderTabs persists a new tab order within a project and emits an
// event. orderedIDs must be the project's full tab set in desired
// visual order.
func (w *Workspace) ReorderTabs(projectID int64, orderedIDs []int64) error {
	if err := w.store.ReorderTabs(projectID, orderedIDs); err != nil {
		return err
	}
	ids := append([]int64(nil), orderedIDs...)
	w.emit(Event{Kind: EventTabsReordered, ProjectID: projectID, OrderedTabIDs: ids})
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

// DeleteTab removes a tab. Captures the tab's ProjectID before delete
// so subscribers (notably the project rollup recompute in the UI) can
// react without doing a separate lookup.
//
// If the tab is already gone (concurrent caller), the event still
// fires with ProjectID=0 and subscribers tolerate that. Any other
// store error from the lookup is propagated so a real DB failure
// surfaces to the caller rather than continuing into the delete.
func (w *Workspace) DeleteTab(id int64) error {
	var projectID int64
	t, err := w.store.GetTab(id)
	switch {
	case err == nil:
		projectID = t.ProjectID
	case errors.Is(err, sql.ErrNoRows):
		// already gone; carry on with projectID==0
	default:
		return err
	}
	if err := w.store.DeleteTab(id); err != nil {
		return err
	}
	w.clearTabState(id)
	w.emit(Event{Kind: EventTabDeleted, TabID: id, ProjectID: projectID})
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
	// MarkNotification is owned by the UI layer (App.handleNotification)
	// because the focused-tab suppression decision lives there; setting
	// the flag here would leave a stale pending-attention state on a
	// tab the user is already looking at.
	return nil
}

// SetTabAgentState writes the sticky agent state for a tab and emits
// EventTabStateChanged on real transitions. Idempotent re-sets are
// silent (no event). Pass TabAgentNone to clear.
func (w *Workspace) SetTabAgentState(tabID int64, state TabAgentState) {
	w.mu.Lock()
	if w.agentState == nil {
		w.agentState = make(map[int64]TabAgentState)
	}
	prev := w.agentState[tabID]
	if prev == state {
		w.mu.Unlock()
		return
	}
	if state == TabAgentNone {
		delete(w.agentState, tabID)
	} else {
		w.agentState[tabID] = state
	}
	w.mu.Unlock()
	w.emit(Event{Kind: EventTabStateChanged, TabID: tabID, AgentState: state})
}

// TabAgentState returns the current sticky state for a tab, or
// TabAgentNone if unknown.
func (w *Workspace) TabAgentState(tabID int64) TabAgentState {
	w.mu.Lock()
	defer w.mu.Unlock()
	if s, ok := w.agentState[tabID]; ok {
		return s
	}
	return TabAgentNone
}

// SetHookSessionActive marks whether a structured hook session (e.g.
// Claude Code's hook subcommand) is currently driving this tab. Used
// by the OSC pump to suppress raw OSC 9/777 from inside the agent —
// the hook is the trusted channel; OSC is the fallback for tools that
// can't be modified, and hook-driven agents emit OSC noise we don't
// want to surface twice.
//
// Pure setter: does not emit an event. The OSC suppression is the
// only consumer.
func (w *Workspace) SetHookSessionActive(tabID int64, active bool) {
	w.mu.Lock()
	defer w.mu.Unlock()
	if w.hookActive == nil {
		w.hookActive = make(map[int64]bool)
	}
	if active {
		w.hookActive[tabID] = true
	} else {
		delete(w.hookActive, tabID)
	}
}

// IsHookSessionActive reports whether a hook session is currently
// driving the tab. Safe to call from any goroutine (used by the PTY
// pump in session.go).
func (w *Workspace) IsHookSessionActive(tabID int64) bool {
	w.mu.Lock()
	defer w.mu.Unlock()
	return w.hookActive[tabID]
}

// MarkNotification flips the per-tab "has pending notification" flag
// and emits EventTabNotificationChanged on transitions. The UI sets
// true via Notify (indirectly) and false from the selected-page
// handler when the user looks at the tab.
func (w *Workspace) MarkNotification(tabID int64, has bool) {
	w.mu.Lock()
	if w.hasNotification == nil {
		w.hasNotification = make(map[int64]bool)
	}
	prev := w.hasNotification[tabID]
	if prev == has {
		w.mu.Unlock()
		return
	}
	if has {
		w.hasNotification[tabID] = true
	} else {
		delete(w.hasNotification, tabID)
	}
	w.mu.Unlock()
	w.emit(Event{Kind: EventTabNotificationChanged, TabID: tabID})
}

// HasNotification reports whether the tab has a pending notification
// the user has not yet seen.
func (w *Workspace) HasNotification(tabID int64) bool {
	w.mu.Lock()
	defer w.mu.Unlock()
	return w.hasNotification[tabID]
}

// clearTabState wipes all in-memory per-tab state. Called from
// DeleteTab so a future tab with the same ID (impossible today; the
// store autoincrements) wouldn't inherit stale entries.
func (w *Workspace) clearTabState(tabID int64) {
	w.mu.Lock()
	defer w.mu.Unlock()
	delete(w.agentState, tabID)
	delete(w.hookActive, tabID)
	delete(w.hasNotification, tabID)
	delete(w.lastNotify, tabID)
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
