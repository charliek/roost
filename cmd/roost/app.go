package main

import (
	"context"
	_ "embed"
	"errors"
	"log/slog"
	"os"
	"runtime"
	"sort"
	"strconv"
	"strings"

	"github.com/diamondburned/gotk4-adwaita/pkg/adw"
	coreglib "github.com/diamondburned/gotk4/pkg/core/glib"
	"github.com/diamondburned/gotk4/pkg/gdk/v4"
	"github.com/diamondburned/gotk4/pkg/gio/v2"
	"github.com/diamondburned/gotk4/pkg/glib/v2"
	"github.com/diamondburned/gotk4/pkg/gtk/v4"

	"github.com/charliek/roost/internal/config"
	"github.com/charliek/roost/internal/core"
	"github.com/charliek/roost/internal/ghostty"
	"github.com/charliek/roost/internal/ipc"
)

//go:embed style.css
var styleCSS string

// App is the top-level UI coordinator. It owns the workspace, every
// open Session, and the widget tree. The App is the only place that
// reads/writes both core state and GTK widgets — packages below it
// never touch widgets, packages above don't touch state.
type App struct {
	gtkApp *adw.Application
	ws     *core.Workspace
	home   string

	// cfg is the parsed user config. Immutable after construction:
	// safe to read from any thread during init, then only from the GTK
	// main thread. If live-reload lands, this needs to grow a mutex.
	cfg config.Config

	// theme is the resolved color scheme used by every Session. Loaded
	// once in NewApp from cfg.Theme so a typo in the config file warns
	// once at startup, not once per tab open.
	theme Theme

	socketPath string
	ipcServer  *ipc.Server

	win *adw.ApplicationWindow

	// Header chrome above the tabs. headerTitle's title shows the active
	// project's name; subtitle shows the active tab's cwd.
	headerIcon  *gtk.Image
	headerTitle *adw.WindowTitle

	// One AdwTabView per project. Stack switches between them so each
	// project keeps its own tab strip + selection state.
	stack        *gtk.Stack
	projectViews map[int64]*adw.TabView

	// The sidebar is a Gtk.ListBox of project rows.
	sidebar     *gtk.ListBox
	projectRows map[int64]*projectRow

	// All open tab sessions, keyed by tab ID.
	sessions map[int64]*Session
	// page->tab lookup. Keyed by the AdwTabPage's underlying GObject
	// pointer (page.Native()) rather than the Go wrapper pointer —
	// gotk4 may hand back a fresh wrapper from getter calls like
	// view.SelectedPage(), and the wrapper's Go identity is not stable
	// across those calls. Use pageKey() to derive the lookup key.
	pageTabs map[uintptr]int64
	// tab->page reverse lookup, used for badge updates from the IPC
	// goroutine via glib.IdleAdd.
	tabPages map[int64]*adw.TabPage

	// stateIcons holds the three colored-circle indicator icons used
	// by AdwTabPage.SetIndicatorIcon. Built once on activate so the
	// per-event handler can swap by reference.
	stateIcons stateIcons

	// terminalNotifierPath is the absolute path of the
	// terminal-notifier binary, resolved once at activate. Empty
	// string means "not installed" — macOS desktop banners become
	// silent no-ops; in-app indicators continue working.
	terminalNotifierPath string
	// roostCLIPath is the absolute path of the roost-cli binary.
	// Used as the click-through target on macOS so terminal-notifier
	// -execute does not depend on PATH at click time. Resolved once
	// at activate.
	roostCLIPath string

	activeProjectID int64
}

// NewApp wires the app together. The window is built in activate.
//
// Theme resolution happens here rather than per-Session: a bad theme
// name in the config produces one warning at startup instead of a
// duplicate warning for every tab opened or rehydrated.
func NewApp(gtkApp *adw.Application, ws *core.Workspace, cfg config.Config, home, socketPath string) *App {
	theme, err := LoadTheme(cfg.Theme)
	if err != nil {
		slog.Warn("theme load failed; using roost-dark", "name", cfg.Theme, "err", err)
		theme = DefaultTheme
	}
	return &App{
		gtkApp:       gtkApp,
		ws:           ws,
		cfg:          cfg,
		theme:        theme,
		home:         home,
		socketPath:   socketPath,
		projectViews: map[int64]*adw.TabView{},
		projectRows:  map[int64]*projectRow{},
		sessions:     map[int64]*Session{},
		pageTabs:     map[uintptr]int64{},
		tabPages:     map[int64]*adw.TabPage{},
	}
}

// activate is wired to AdwApplication::activate. Builds the entire
// window content and rehydrates persistent state into the UI.
func (a *App) activate() {
	// Terminals are dark-by-convention; force dark so libadwaita's accent
	// colors land strongly against the dark surface.
	adw.StyleManagerGetDefault().SetColorScheme(adw.ColorSchemePreferDark)

	// Per-state indicator icons. Built once; tab updates swap by reference.
	a.stateIcons = newStateIcons()

	// Resolve external-binary paths once. Empty terminal-notifier
	// means "no macOS banners" (logged below); empty roost-cli only
	// hurts the click-through target on macOS — Linux uses an
	// in-process action handler.
	if runtime.GOOS == "darwin" {
		a.terminalNotifierPath = lookupTerminalNotifier()
		if a.terminalNotifierPath == "" {
			slog.Warn("terminal-notifier not found on PATH; macOS desktop banners disabled. Install with: brew install terminal-notifier")
		}
	}
	a.roostCLIPath = lookupRoostCLI()

	// Register the app-level "tab-focus" GIO action. The Linux
	// gio.Notification default action invokes this in-process; the
	// signal ferries the tab id as an int64 variant. macOS uses a
	// different transport (terminal-notifier -execute) so this
	// handler is unused there, but it's cheap to register on both.
	{
		focusAct := gio.NewSimpleAction("tab-focus", glib.NewVariantType("x"))
		focusAct.ConnectActivate(func(param *glib.Variant) {
			if param == nil {
				return
			}
			tabID := param.Int64()
			go func() {
				if _, err := a.FocusTab(tabID); err != nil {
					slog.Warn("tab-focus action", "tab", tabID, "err", err)
				}
			}()
		})
		a.gtkApp.AddAction(focusAct)
	}

	a.win = adw.NewApplicationWindow(&a.gtkApp.Application)
	a.win.SetTitle("Roost")
	a.win.SetDefaultSize(1200, 780)

	// Application-level CSS overrides. Keep this small; see style.css.
	if display := gdk.DisplayGetDefault(); display != nil {
		provider := gtk.NewCSSProvider()
		provider.LoadFromString(styleCSS)
		gtk.StyleContextAddProviderForDisplay(display, provider, gtk.STYLE_PROVIDER_PRIORITY_APPLICATION)
	}

	// HeaderBar: folder icon on the left, AdwWindowTitle reflecting the
	// active project + active tab cwd, "+ tab" on the right. The flat
	// style class drops the bottom border so the dark header reads as
	// part of the window chrome.
	header := adw.NewHeaderBar()
	header.AddCSSClass("flat")
	a.headerIcon = gtk.NewImageFromIconName("folder-symbolic")
	a.headerIcon.SetMarginEnd(4)
	header.PackStart(a.headerIcon)
	a.headerTitle = adw.NewWindowTitle("Roost", "")
	header.SetTitleWidget(a.headerTitle)

	newTabBtn := gtk.NewButtonFromIconName("tab-new-symbolic")
	newTabTip := "New tab in current project (Ctrl-T)"
	if runtime.GOOS == "darwin" {
		newTabTip = "New tab in current project (Cmd-T)"
	}
	newTabBtn.SetTooltipText(newTabTip)
	newTabBtn.ConnectClicked(func() { a.newTabInActiveProject() })
	header.PackEnd(newTabBtn)

	// Sidebar.
	a.sidebar = gtk.NewListBox()
	a.sidebar.SetSelectionMode(gtk.SelectionBrowse)
	a.sidebar.AddCSSClass("navigation-sidebar")
	a.sidebar.ConnectRowSelected(func(row *gtk.ListBoxRow) {
		if row == nil {
			return
		}
		pid := int64(row.Index()) // we'll override via name attr below
		if v := row.Name(); v != "" {
			if id, err := strconv.ParseInt(v, 10, 64); err == nil {
				pid = id
			}
		}
		a.selectProject(pid)
	})
	sidebarScroll := gtk.NewScrolledWindow()
	sidebarScroll.SetChild(a.sidebar)
	sidebarScroll.SetSizeRequest(220, -1)

	newProjectBtn := gtk.NewButtonWithLabel("+ Project")
	newProjectBtn.SetMarginTop(8)
	newProjectBtn.SetMarginBottom(8)
	newProjectBtn.SetMarginStart(8)
	newProjectBtn.SetMarginEnd(8)
	newProjectBtn.ConnectClicked(func() { a.newProject() })

	sectionHeader := gtk.NewLabel("Projects")
	sectionHeader.SetXAlign(0)
	sectionHeader.AddCSSClass("sidebar-section-header")

	sidebarBox := gtk.NewBox(gtk.OrientationVertical, 0)
	sidebarBox.Append(sectionHeader)
	sidebarBox.Append(sidebarScroll)
	sidebarBox.SetVExpand(true)
	sidebarScroll.SetVExpand(true)
	sidebarBox.Append(newProjectBtn)

	// Right side: a Stack of AdwTabView (one per project) plus an empty
	// state for when there are no projects.
	a.stack = gtk.NewStack()
	a.stack.SetHExpand(true)
	a.stack.SetVExpand(true)
	a.stack.AddNamed(a.buildEmptyState(), emptyStackName)

	paned := gtk.NewPaned(gtk.OrientationHorizontal)
	paned.SetStartChild(sidebarBox)
	paned.SetEndChild(a.stack)
	paned.SetResizeStartChild(false)
	paned.SetShrinkStartChild(false)
	paned.SetPosition(220)

	// HeaderBar at the very top of the window so GTK reserves space for
	// the macOS traffic lights and the sidebar doesn't get clipped.
	root := gtk.NewBox(gtk.OrientationVertical, 0)
	root.Append(header)
	root.Append(paned)
	a.win.SetContent(root)

	a.installShortcuts()
	a.subscribeWorkspace()
	a.rehydrate()
	a.startIPC()

	a.win.ConnectCloseRequest(func() bool {
		a.shutdown()
		return false
	})

	// Pause cursor blink and degrade to a hollow outline when the
	// window loses focus. Notifies every session because the cursor
	// state is per-session.
	a.win.NotifyProperty("is-active", func() {
		focused := a.win.IsActive()
		for _, sess := range a.sessions {
			sess.SetWindowFocused(focused)
		}
	})

	a.win.Present()
}

// startIPC opens the Unix socket server. Failure is logged but
// non-fatal — the GUI still works without the companion CLI.
func (a *App) startIPC() {
	a.ipcServer = ipc.NewServer(a.socketPath, a)
	if err := a.ipcServer.Start(); err != nil {
		slog.Error("ipc start", "err", err)
		a.ipcServer = nil
		return
	}
	slog.Info("ipc listening", "socket", a.socketPath)
}

// subscribeWorkspace bridges core events to the UI. Subscribers run on
// the goroutine that triggered the event, so each handler marshals to
// the GTK main thread via glib.IdleAdd before touching widgets.
func (a *App) subscribeWorkspace() {
	ch := a.ws.Subscribe(64)
	go func() {
		for ev := range ch {
			ev := ev
			coreglib.IdleAdd(func() bool {
				a.handleEvent(ev)
				return false
			})
		}
	}()
}

func (a *App) handleEvent(ev core.Event) {
	switch ev.Kind {
	case core.EventNotification:
		a.handleNotification(ev.TabID, ev.Title, ev.Body)
	case core.EventTabUpdated:
		// Payload is partial — see Event doc-comment in internal/core.
		// Title is the only field with a UI surface today; CWD lives
		// only in the store. Guard on Title != "" to skip CWD-only
		// updates without overwriting the displayed title with empty.
		if ev.Tab == nil || ev.Tab.Title == "" {
			return
		}
		if page, ok := a.tabPages[ev.Tab.ID]; ok {
			page.SetTitle(ev.Tab.Title)
		}
		// Keep the in-memory Session.tab title in sync so the OSC-empty
		// fallback at session.go's onTitleChanged uses the latest title
		// rather than the value from session creation. UserTitled is
		// set-only here: SetTabTitleFromOSC's event leaves the field
		// false, so we must not infer "unlocked" from absence — only
		// propagate true. (See Event doc-comment in internal/core.)
		if sess, ok := a.sessions[ev.Tab.ID]; ok {
			sess.tab.Title = ev.Tab.Title
			if ev.Tab.UserTitled {
				sess.tab.UserTitled = true
			}
		}
	case core.EventProjectRenamed:
		if ev.Project == nil {
			return
		}
		if pr, ok := a.projectRows[ev.Project.ID]; ok {
			pr.setName(ev.Project.Name)
		}
		if a.activeProjectID == ev.Project.ID {
			a.updateHeader()
		}
	case core.EventProjectDeleted:
		a.removeProjectUI(ev.ProjectID)
	case core.EventTabAdded:
		// New tab — recompute the rollup for its owning project so a
		// fresh tab in agent-state none doesn't leave a stale stripe.
		if ev.Tab != nil {
			a.recomputeProjectRollup(ev.Tab.ProjectID)
		}
	case core.EventTabDeleted:
		// EventTabDeleted now carries ProjectID — recompute so a
		// freshly cleaned project loses its stripe.
		if ev.ProjectID != 0 {
			a.recomputeProjectRollup(ev.ProjectID)
		}
	case core.EventTabStateChanged:
		if page, ok := a.tabPages[ev.TabID]; ok {
			// Indicator-icon and SetNeedsAttention are independent
			// surfaces: state lives in the indicator slot; the
			// "needs attention" pulse is driven by the notification
			// flag elsewhere.
			page.SetIndicatorIcon(a.indicatorIconForState(ev.AgentState))
		}
		if pid, ok := a.projectForTab(ev.TabID); ok {
			a.recomputeProjectRollup(pid)
		}
	case core.EventTabNotificationChanged:
		// The notification flag flipped. handleNotification calls
		// SetNeedsAttention(true) directly when a fresh notification
		// arrives; this case handles the clear path so a hook event
		// (claude-hook prompt-submit) or an explicit
		// tab.clear_notification can also turn the pulse off without
		// requiring the user to focus the tab.
		if !a.ws.HasNotification(ev.TabID) {
			if page, ok := a.tabPages[ev.TabID]; ok {
				page.SetNeedsAttention(false)
			}
		}
	}
}

// handleNotification updates tab + project visual state when something
// notifies us. Active tab is left alone (you're already looking at it).
func (a *App) handleNotification(tabID int64, title, body string) {
	page, ok := a.tabPages[tabID]
	if !ok {
		slog.Warn("notification for unknown tab", "tab", tabID)
		return
	}
	if a.tabIsActive(tabID) {
		// Already focused — no badge, no desktop notification, no
		// pending-attention flag. Notify already emitted
		// EventNotification; we just don't escalate any of the
		// surfaces the user can already see.
		slog.Info("notification (suppressed; tab active)", "tab", tabID, "title", title)
		return
	}
	page.SetNeedsAttention(true)
	// Mark the in-core flag too — drives the rollup recompute and any
	// future surfaces (sidebar dot count, inbox) listening on
	// EventTabNotificationChanged. Owned by the UI layer because the
	// focused-tab decision is here.
	a.ws.MarkNotification(tabID, true)

	id := "roost.tab." + strconv.FormatInt(tabID, 10)
	sendDesktopNotification(a.gtkApp, id, tabID, title, body, a.terminalNotifierPath, a.roostCLIPath)
	slog.Info("notification", "tab", tabID, "title", title, "body", body)
}

func (a *App) tabIsActive(tabID int64) bool {
	view := a.projectViews[a.activeProjectID]
	if view == nil {
		return false
	}
	page := view.SelectedPage()
	if page == nil {
		return false
	}
	if id, ok := a.pageTabs[pageKey(page)]; ok {
		return id == tabID && a.win.IsActive()
	}
	return false
}

// --- ipc.Handler --------------------------------------------------------

func (a *App) Notify(tabID int64, title, body string) error {
	if tabID == 0 {
		return errors.New("tab_id required (set ROOST_TAB_ID or pass --tab)")
	}
	return a.ws.Notify(tabID, title, body)
}

func (a *App) SetTitle(tabID int64, title string) error {
	if tabID == 0 {
		return errors.New("tab_id required (set ROOST_TAB_ID or pass --tab)")
	}
	// CLI title-set is user intent — lock the tab against subsequent OSC
	// overwrites just like the Cmd-R popover does.
	return a.ws.RenameTab(tabID, title)
}

// FocusTab switches the active project to the tab's owner, selects
// the tab inside that project's view, and raises the window. Returns
// the previously focused (project, tab) so a client can implement
// "go back."
//
// Caveat: on Wayland, win.Present() without an XDG-activation token
// may only flash the taskbar instead of raising. The terminal-notifier
// click path passes a token through; CLI scripts that call this
// directly may not. Best-effort.
func (a *App) FocusTab(tabID int64) (ipc.TabFocusResult, error) {
	type result struct {
		res ipc.TabFocusResult
		err error
	}
	done := make(chan result, 1)
	coreglib.IdleAdd(func() bool {
		page, ok := a.tabPages[tabID]
		if !ok {
			done <- result{err: errors.New("tab not found")}
			return false
		}
		// Capture previous focus before switching.
		prev := ipc.TabFocusResult{PreviousProjectID: a.activeProjectID}
		if view := a.projectViews[a.activeProjectID]; view != nil {
			if p := view.SelectedPage(); p != nil {
				if id, ok := a.pageTabs[pageKey(p)]; ok {
					prev.PreviousTabID = id
				}
			}
		}

		ownerProject, ok := a.projectForTab(tabID)
		if !ok {
			done <- result{err: errors.New("tab owner project not found")}
			return false
		}
		a.selectProject(ownerProject)
		if view := a.projectViews[ownerProject]; view != nil {
			view.SetSelectedPage(page)
		}
		// Selected-page handler clears needs-attention + grabs DA focus
		// when the selection changes. If the target tab was already
		// the project's selected page (e.g. you switched away from this
		// project earlier without picking a different tab), no
		// selected-page event fires — clear the surfaces explicitly.
		page.SetNeedsAttention(false)
		a.ws.MarkNotification(tabID, false)
		a.win.Present()
		done <- result{res: prev}
		return false
	})
	r := <-done
	return r.res, r.err
}

// projectForTab returns the project ID that owns a tab. Reads
// in-memory state populated on the main thread; callers must already
// be on the main thread.
func (a *App) projectForTab(tabID int64) (int64, bool) {
	if sess, ok := a.sessions[tabID]; ok {
		return sess.tab.ProjectID, true
	}
	return 0, false
}

// rollupSeverity returns a comparable rank for project rollup
// computation. needs-input dominates because it's the most actionable
// state: a project with one blocked tab and four running tabs should
// flag the user, not look "busy."
func rollupSeverity(s core.TabAgentState) int {
	switch s {
	case core.TabAgentNeedsInput:
		return 3
	case core.TabAgentRunning:
		return 2
	case core.TabAgentIdle:
		return 1
	}
	return 0
}

// recomputeProjectRollup walks every tab in the project's TabView,
// asks core for its agent state, and applies the highest-severity
// state to the project's sidebar row. Cheap: O(tabs in project) of
// in-memory map lookups. Called from the event handler on every
// state change that could affect the rollup.
//
// Must run on the main thread (touches GTK widgets).
func (a *App) recomputeProjectRollup(projectID int64) {
	view, ok := a.projectViews[projectID]
	if !ok {
		return
	}
	pr, ok := a.projectRows[projectID]
	if !ok {
		return
	}
	rollup := core.TabAgentNone
	for i := 0; i < view.NPages(); i++ {
		page := view.NthPage(i)
		tabID, ok := a.pageTabs[pageKey(page)]
		if !ok {
			continue
		}
		s := a.ws.TabAgentState(tabID)
		if rollupSeverity(s) > rollupSeverity(rollup) {
			rollup = s
		}
	}
	pr.setRollupState(rollup)
}

// ListTabs returns a project-grouped tree of every tab. Display order
// for projects matches the sidebar; tab order within a project matches
// the visible AdwTabView. Marshalled onto the main thread to keep the
// widget reads consistent.
func (a *App) ListTabs() (ipc.TabListResult, error) {
	type result struct {
		res ipc.TabListResult
		err error
	}
	done := make(chan result, 1)
	coreglib.IdleAdd(func() bool {
		// Project order comes from the store (sidebar mirrors store
		// position). Map iteration would be non-deterministic.
		snap, err := a.ws.LoadAll()
		if err != nil {
			done <- result{err: err}
			return false
		}
		var activeTabID int64
		if view := a.projectViews[a.activeProjectID]; view != nil {
			if p := view.SelectedPage(); p != nil {
				if id, ok := a.pageTabs[pageKey(p)]; ok {
					activeTabID = id
				}
			}
		}
		out := ipc.TabListResult{Projects: make([]ipc.TabListProject, 0, len(snap))}
		for _, ps := range snap {
			view := a.projectViews[ps.Project.ID]
			tabsOut := make([]ipc.TabListTab, 0, len(ps.Tabs))
			// Walk the live tab view (matches visual order). Fall back to
			// store order if the view hasn't been built yet.
			if view != nil {
				for i := 0; i < view.NPages(); i++ {
					page := view.NthPage(i)
					id, ok := a.pageTabs[pageKey(page)]
					if !ok {
						continue
					}
					title := page.Title()
					tabsOut = append(tabsOut, ipc.TabListTab{
						ID:              id,
						Title:           title,
						AgentState:      string(a.ws.TabAgentState(id)),
						HasNotification: a.ws.HasNotification(id),
						IsActive:        ps.Project.ID == a.activeProjectID && id == activeTabID,
					})
				}
			} else {
				for _, t := range ps.Tabs {
					tabsOut = append(tabsOut, ipc.TabListTab{
						ID:              t.ID,
						Title:           t.Title,
						AgentState:      string(a.ws.TabAgentState(t.ID)),
						HasNotification: a.ws.HasNotification(t.ID),
					})
				}
			}
			out.Projects = append(out.Projects, ipc.TabListProject{
				ID:   ps.Project.ID,
				Name: ps.Project.Name,
				Tabs: tabsOut,
			})
		}
		done <- result{res: out}
		return false
	})
	r := <-done
	return r.res, r.err
}

// SetTabState writes the sticky agent state. The IPC server already
// validated `state`; treat the workspace setter as the authority on
// idempotency / event emission.
func (a *App) SetTabState(tabID int64, state string) error {
	a.ws.SetTabAgentState(tabID, core.TabAgentState(state))
	return nil
}

// ClearTabNotification flips the per-tab pending-notification flag
// off. Used by the prompt-submit hook so a fresh prompt clears any
// stale awaiting-input badge.
func (a *App) ClearTabNotification(tabID int64) error {
	a.ws.MarkNotification(tabID, false)
	return nil
}

// SetHookActive toggles the per-tab hook-session-active flag. While
// true, raw OSC 9/777 from inside the tab is suppressed by the PTY
// pump (see session.go OnNotification).
func (a *App) SetHookActive(tabID int64, active bool) error {
	a.ws.SetHookSessionActive(tabID, active)
	return nil
}

// Identify is called from the IPC server's per-connection goroutine
// but reads activeProjectID, projectViews, and pageTabs which are
// otherwise mutated only on the GTK main thread. Marshal the body
// onto the main thread and block on a result channel to keep the
// reads consistent.
func (a *App) Identify() ipc.Identity {
	done := make(chan ipc.Identity, 1)
	coreglib.IdleAdd(func() bool {
		id := ipc.Identity{
			SocketPath:      a.socketPath,
			PID:             os.Getpid(),
			ActiveProjectID: a.activeProjectID,
		}
		if view := a.projectViews[a.activeProjectID]; view != nil {
			if page := view.SelectedPage(); page != nil {
				if tid, ok := a.pageTabs[pageKey(page)]; ok {
					id.ActiveTabID = tid
				}
			}
		}
		done <- id
		return false
	})
	return <-done
}

// rehydrate reads every persisted project + tab and recreates UI for
// each. Selects the first project and its first tab as active.
func (a *App) rehydrate() {
	projects, err := a.ws.LoadAll()
	if err != nil {
		slog.Error("rehydrate", "err", err)
		return
	}
	for _, p := range projects {
		a.addProjectUI(p.Project)
		for _, t := range p.Tabs {
			a.addTabUI(p.Project.ID, t)
		}
	}
	if len(projects) > 0 {
		a.selectProject(projects[0].Project.ID)
	} else {
		a.stack.SetVisibleChildName(emptyStackName)
		a.updateHeader()
	}
}

// addProjectUI creates a sidebar row + a TabView in the stack for one
// project. Idempotent: bails out if the project is already in the UI.
func (a *App) addProjectUI(p core.Project) {
	if _, ok := a.projectViews[p.ID]; ok {
		return
	}
	pid := p.ID
	pr := newProjectRow(p.Name)
	pr.row.SetName(strconv.FormatInt(pid, 10))
	pr.installEditControllers(
		func(text string) { a.commitRename(pid, text) },
		func() {},
	)
	pr.closeBtn.ConnectClicked(func() { a.requestCloseProject(pid) })

	// Action group bound to the row once. Used by the right-click
	// popover menu so we don't allocate + insert a new group on every
	// right-click.
	group := gio.NewSimpleActionGroup()
	renameAct := gio.NewSimpleAction("rename", nil)
	renameAct.ConnectActivate(func(_ *glib.Variant) { pr.enterEditMode() })
	group.AddAction(renameAct)
	closeAct := gio.NewSimpleAction("close", nil)
	closeAct.ConnectActivate(func(_ *glib.Variant) { a.requestCloseProject(pid) })
	group.AddAction(closeAct)
	pr.row.InsertActionGroup("row", group)

	// Double-click on the label area enters rename mode.
	dbl := gtk.NewGestureClick()
	dbl.SetButton(1)
	dbl.ConnectPressed(func(nPress int, _, _ float64) {
		if nPress == 2 {
			pr.enterEditMode()
		}
	})
	pr.label.AddController(dbl)

	// Right-click anywhere on the row opens a Rename / Close menu.
	right := gtk.NewGestureClick()
	right.SetButton(3)
	right.ConnectPressed(func(_ int, _, _ float64) {
		a.showRowMenu(pr)
	})
	pr.row.AddController(right)

	a.sidebar.Append(pr.row)
	a.projectRows[pid] = pr

	view := adw.NewTabView()
	view.ConnectClosePage(func(page *adw.TabPage) bool {
		ownerPID := a.projectIDForPage(page)
		a.closeTab(page)
		view.ClosePageFinish(page, true)
		// When a project's last tab closes (whether via UI close, Cmd-W /
		// Ctrl-W, or a shell exiting), close the project silently. The "are you
		// sure" dialog only fires for explicit close-project clicks.
		if v := a.projectViews[ownerPID]; v != nil && v.NPages() == 0 {
			a.deleteProject(ownerPID)
		}
		return true // we handled it
	})
	view.NotifyProperty("selected-page", func() {
		page := view.SelectedPage()
		if page == nil {
			return
		}
		page.SetNeedsAttention(false) // user's looking at it now
		if id, ok := a.pageTabs[pageKey(page)]; ok {
			// Clear the in-core notification flag too — emits
			// EventTabNotificationChanged which the rollup
			// recompute (commit 6) listens to. State (running /
			// needs_input / idle) is *not* cleared on focus; only
			// hook events change it.
			a.ws.MarkNotification(id, false)
			if sess, ok := a.sessions[id]; ok {
				sess.da.GrabFocus()
			}
		}
		if a.activeProjectID == pid {
			a.updateHeader()
		}
	})

	bar := adw.NewTabBar()
	bar.SetView(view)
	bar.SetAutohide(false)

	box := gtk.NewBox(gtk.OrientationVertical, 0)
	box.Append(bar)
	box.Append(view)

	stackName := strconv.FormatInt(pid, 10)
	a.stack.AddNamed(box, stackName)
	a.projectViews[pid] = view
}

// addTabUI creates a Session for the tab and adds a tab page to the
// project's TabView. Injects ROOST_TAB_ID + ROOST_SOCKET into the
// shell's env so the companion CLI inside the tab can call back.
func (a *App) addTabUI(projectID int64, tab core.Tab) {
	view, ok := a.projectViews[projectID]
	if !ok {
		slog.Warn("addTabUI: missing project view", "project", projectID)
		return
	}
	env := []string{
		"ROOST_TAB_ID=" + strconv.FormatInt(tab.ID, 10),
		"ROOST_PROJECT_ID=" + strconv.FormatInt(projectID, 10),
		"ROOST_SOCKET=" + a.socketPath,
	}
	sess, err := NewSession(a.ws, tab, initialCols, initialRows, BuildFontConfig(a.cfg), a.theme, env...)
	if err != nil {
		slog.Error("NewSession", "tab", tab.ID, "err", err)
		return
	}
	a.sessions[tab.ID] = sess

	page := view.Append(sess.DrawingArea())
	page.SetTitle(displayTitle(tab))
	page.SetLiveThumbnail(false)
	a.pageTabs[pageKey(page)] = tab.ID
	a.tabPages[tab.ID] = page

	// OSC 0/1/2 title updates → refresh tab page + persist, except when
	// the tab is user-locked. The visible-flash gate matters: without
	// it, we'd briefly call page.SetTitle(title) before the persist
	// no-ops, showing the OSC title for a frame on a locked tab.
	tabID := tab.ID
	sess.onTitleChanged = func(title string) {
		if title == "" {
			page.SetTitle(displayTitle(sess.tab))
			return
		}
		if sess.tab.UserTitled {
			return
		}
		page.SetTitle(title)
		if err := a.ws.SetTabTitleFromOSC(tabID, title); err != nil {
			slog.Warn("SetTabTitleFromOSC", "tab", tabID, "title", title, "err", err)
		}
	}
	// OSC 7 cwd updates → persist for next-launch shell spawn, and
	// refresh the header subtitle when this is the active tab.
	sess.onPWDChanged = func(cwd string) {
		if cwd == "" {
			return
		}
		if err := a.ws.UpdateTabCWD(tabID, cwd); err != nil {
			slog.Warn("UpdateTabCWD", "tab", tabID, "cwd", cwd, "err", err)
		}
		if a.tabIsSelected(tabID) {
			a.updateHeader()
		}
	}
	// Shell exit (typing `exit`, the process dying) closes the tab
	// without leaving an empty pane. The view.ClosePage call routes
	// through ConnectClosePage above, which frees the session and, if
	// it was the last tab, the project.
	sess.onPTYExit = func() {
		view.ClosePage(page)
	}

	// Clearing the badge when the user actually looks at the tab is
	// done in the project view's selected-page notify handler below.
}

// tabIsSelected returns true if tabID is the active project's selected
// tab — used to decide whether a per-tab event (e.g. cwd change) should
// refresh the global header.
func (a *App) tabIsSelected(tabID int64) bool {
	view := a.projectViews[a.activeProjectID]
	if view == nil {
		return false
	}
	page := view.SelectedPage()
	if page == nil {
		return false
	}
	id, ok := a.pageTabs[pageKey(page)]
	return ok && id == tabID
}

func displayTitle(t core.Tab) string {
	if t.Title != "" {
		return t.Title
	}
	return shorten(t.CWD, 22)
}

// shorten clamps a string to max display characters, prepending an
// ellipsis if it had to be cut. Iterates runes rather than bytes so
// CJK, emoji, and accented characters don't get sliced mid-codepoint.
func shorten(s string, max int) string {
	runes := []rune(s)
	if len(runes) <= max {
		return s
	}
	return "…" + string(runes[len(runes)-max+1:])
}

// selectProject switches the right-side Stack to the given project's
// TabView and updates the activeProjectID.
func (a *App) selectProject(projectID int64) {
	if _, ok := a.projectViews[projectID]; !ok {
		return
	}
	if a.activeProjectID == projectID {
		a.updateHeader()
		return
	}
	a.activeProjectID = projectID
	a.stack.SetVisibleChildName(strconv.FormatInt(projectID, 10))

	// Sync sidebar selection (avoids feedback loop because ListBox
	// dedupes selecting an already-selected row).
	if pr, ok := a.projectRows[projectID]; ok {
		a.sidebar.SelectRow(pr.row)
	}

	// Focus the active tab so keystrokes go to the terminal. Also clear
	// any pending notification on the now-visible tab — switching to a
	// project whose currently-selected tab is the target wouldn't fire
	// that view's selected-page handler (the selection didn't change),
	// so the badge would persist even though the user is looking at it.
	view := a.projectViews[projectID]
	if page := view.SelectedPage(); page != nil {
		if id, ok := a.pageTabs[pageKey(page)]; ok {
			page.SetNeedsAttention(false)
			a.ws.MarkNotification(id, false)
			if sess, ok := a.sessions[id]; ok {
				sess.da.GrabFocus()
			}
		}
	}
	a.updateHeader()
}

// updateHeader refreshes the AdwWindowTitle in the headerbar with the
// active project's name and the active tab's cwd. Called on project
// switch, tab switch, and cwd change. Falls back to "Roost" / "" when
// there's no active project (empty state).
func (a *App) updateHeader() {
	if a.headerTitle == nil {
		return
	}
	if a.activeProjectID == 0 {
		a.headerTitle.SetTitle("Roost")
		a.headerTitle.SetSubtitle("")
		if a.headerIcon != nil {
			a.headerIcon.SetVisible(false)
		}
		return
	}
	if a.headerIcon != nil {
		a.headerIcon.SetVisible(true)
	}
	if pr, ok := a.projectRows[a.activeProjectID]; ok {
		a.headerTitle.SetTitle(pr.label.Text())
	}
	subtitle := ""
	if view := a.projectViews[a.activeProjectID]; view != nil {
		if page := view.SelectedPage(); page != nil {
			if id, ok := a.pageTabs[pageKey(page)]; ok {
				if sess, ok := a.sessions[id]; ok {
					// lastPWD is the live cwd from OSC 7; tab.CWD is
					// only the snapshot at session creation. Use the
					// live value when we have one so the subtitle
					// follows `cd` without waiting for a tab switch.
					cwd := sess.lastPWD
					if cwd == "" {
						cwd = sess.tab.CWD
					}
					subtitle = shorten(cwd, 48)
				}
			}
		}
	}
	a.headerTitle.SetSubtitle(subtitle)
}

// newProject creates a new project with an auto-generated placeholder
// name and immediately puts the sidebar row into rename mode so the
// user can name it before doing anything else.
func (a *App) newProject() {
	name := nextProjectName(a.projectsByName())
	p, err := a.ws.CreateProject(name, a.home)
	if err != nil {
		slog.Error("CreateProject", "err", err)
		return
	}
	a.addProjectUI(p)
	tab, err := a.ws.CreateTab(p.ID, p.CWD)
	if err != nil {
		slog.Error("CreateTab", "err", err)
		return
	}
	a.addTabUI(p.ID, tab)
	a.selectProject(p.ID)
	if pr, ok := a.projectRows[p.ID]; ok {
		pr.enterEditMode()
	}
}

func (a *App) projectsByName() map[string]bool {
	out := map[string]bool{}
	for _, pr := range a.projectRows {
		out[pr.label.Text()] = true
	}
	return out
}

// nextProjectName picks "untitled", then "untitled 2", "untitled 3" ...
func nextProjectName(taken map[string]bool) string {
	base := "untitled"
	if !taken[base] {
		return base
	}
	for i := 2; i < 1000; i++ {
		n := base + " " + strconv.Itoa(i)
		if !taken[n] {
			return n
		}
	}
	return base
}

// newTabInActiveProject creates a tab in the currently active project
// and opens it in the UI.
func (a *App) newTabInActiveProject() {
	if a.activeProjectID == 0 {
		return
	}
	cwd := a.home
	// Inherit the active tab's *current* cwd. Prefer lastPWD (live OSC 7)
	// so Cmd-T / Ctrl-T after `cd` opens in the new directory; tab.CWD is
	// only the snapshot at session creation.
	if view := a.projectViews[a.activeProjectID]; view != nil {
		if page := view.SelectedPage(); page != nil {
			if id, ok := a.pageTabs[pageKey(page)]; ok {
				if sess, ok := a.sessions[id]; ok {
					cwd = sess.lastPWD
					if cwd == "" {
						cwd = sess.tab.CWD
					}
				}
			}
		}
	}
	tab, err := a.ws.CreateTab(a.activeProjectID, cwd)
	if err != nil {
		slog.Error("CreateTab", "err", err)
		return
	}
	a.addTabUI(a.activeProjectID, tab)
	if view := a.projectViews[a.activeProjectID]; view != nil {
		// AdwTabView.SelectedPage is set by the page that was just
		// added if there was no prior selection; explicitly select
		// the new page so the user lands on it.
		if pages := view.NPages(); pages > 0 {
			page := view.NthPage(pages - 1)
			view.SetSelectedPage(page)
		}
	}
}

// projectIDForPage returns the project the page belongs to. Resolved
// from the session's tab if available; falls back to the active
// project. Used by the ConnectClosePage handler to remember which
// project to refill after the close finishes.
func (a *App) projectIDForPage(page *adw.TabPage) int64 {
	if tabID, ok := a.pageTabs[pageKey(page)]; ok {
		if sess, ok := a.sessions[tabID]; ok {
			return sess.tab.ProjectID
		}
	}
	return a.activeProjectID
}

// closeTab handles AdwTabView::close-page. Frees the Session and
// deletes the tab from the store. The zero-tab fallback lives in the
// ConnectClosePage handler so it can run *after* ClosePageFinish
// removes the page from the view.
func (a *App) closeTab(page *adw.TabPage) {
	tabID, ok := a.pageTabs[pageKey(page)]
	if !ok {
		return
	}
	delete(a.pageTabs, pageKey(page))
	delete(a.tabPages, tabID)

	if sess, ok := a.sessions[tabID]; ok {
		sess.Close()
		delete(a.sessions, tabID)
	}

	if err := a.ws.DeleteTab(tabID); err != nil {
		slog.Error("DeleteTab", "err", err)
	}
}

// closeActiveTab is bound to Cmd-W on macOS, Ctrl-W on Linux. Closes
// the currently selected page.
func (a *App) closeActiveTab() {
	if view := a.projectViews[a.activeProjectID]; view != nil {
		if page := view.SelectedPage(); page != nil {
			view.ClosePage(page) // routes back through ConnectClosePage
		}
	}
}

// cycleTab switches to the next/prev tab in the active project.
func (a *App) cycleTab(delta int) {
	view := a.projectViews[a.activeProjectID]
	if view == nil {
		return
	}
	cur := view.SelectedPage()
	if cur == nil {
		return
	}
	idx := view.PagePosition(cur)
	n := view.NPages()
	if n == 0 {
		return
	}
	next := (idx + int(delta)%int(n) + int(n)) % int(n)
	page := view.NthPage(next)
	view.SetSelectedPage(page)
}

// switchProjectByIndex picks the project at zero-based index in the
// sidebar (Cmd-1..9 on macOS, Alt-1..9 on Linux).
func (a *App) switchProjectByIndex(idx int) {
	row := a.sidebar.RowAtIndex(idx)
	if row == nil {
		return
	}
	v := row.Name()
	if v == "" {
		return
	}
	if id, err := strconv.ParseInt(v, 10, 64); err == nil {
		a.selectProject(id)
	}
}

// switchTabByIndex picks the tab at zero-based index in the active
// project's tab strip (Ctrl-1..9 on both platforms).
func (a *App) switchTabByIndex(idx int) {
	view := a.projectViews[a.activeProjectID]
	if view == nil {
		return
	}
	if idx < 0 || idx >= int(view.NPages()) {
		return
	}
	if page := view.NthPage(int(idx)); page != nil {
		view.SetSelectedPage(page)
	}
}

// renameActiveTab opens a popover with a GtkEntry to rename the active
// tab. Bound to Cmd-R on macOS and Alt-R on Linux. The rename routes
// through Workspace.RenameTab which sets the user-titled lock — once
// renamed, OSC 1/2 from the shell stops overwriting the title.
func (a *App) renameActiveTab() {
	view := a.projectViews[a.activeProjectID]
	if view == nil {
		return
	}
	page := view.SelectedPage()
	if page == nil {
		return
	}
	tabID, ok := a.pageTabs[pageKey(page)]
	if !ok {
		return
	}

	entry := gtk.NewEntry()
	entry.SetText(page.Title())
	entry.SelectRegion(0, -1)

	popover := gtk.NewPopover()
	popover.SetChild(entry)
	popover.SetParent(a.headerTitle)
	popover.SetAutohide(true)
	popover.SetHasArrow(true)

	commit := func() {
		text := strings.TrimSpace(entry.Text())
		popover.Popdown()
		if text == "" || text == page.Title() {
			return
		}
		if err := a.ws.RenameTab(tabID, text); err != nil {
			slog.Error("RenameTab", "tab", tabID, "err", err)
		}
	}
	entry.ConnectActivate(commit)

	keyCtrl := gtk.NewEventControllerKey()
	keyCtrl.ConnectKeyPressed(func(keyval, _ uint, _ gdk.ModifierType) bool {
		if keyval == gdk.KEY_Escape {
			popover.Popdown()
			return true
		}
		return false
	})
	entry.AddController(keyCtrl)

	popover.ConnectClosed(func() { popover.Unparent() })

	popover.Popup()
	entry.GrabFocus()
}

// shutdown closes every Session cleanly. Called on window close.
func (a *App) shutdown() {
	if a.ipcServer != nil {
		_ = a.ipcServer.Close()
	}
	for _, s := range a.sessions {
		s.Close()
	}
}

// installShortcuts wires the app's keyboard shortcuts on the window.
// The ShortcutController is set to PhaseCapture so it runs *before* the
// drawing area's key controller — otherwise terminal-focused keys get
// consumed by handleKey and the shortcut never fires.
//
// Per-platform modifier policy (defaults; overridable via the config
// file's `keybind = trigger=action` lines):
//   - macOS: super (Cmd) is the primary app modifier and the
//     project-management modifier; ctrl is reserved for tab-switch.
//   - Linux: ctrl is the primary app modifier; alt is the project
//     modifier; ctrl is also tab-switch.
//
// On macOS gdk-macos translates NSEventModifierFlagCommand directly to
// GDK_META_MASK, so <Meta>x in a trigger reliably matches Cmd-x. The
// <Primary> alias is hardcoded to Control on every platform in GTK4, so
// the trigger parser substitutes the modifier ourselves.
func (a *App) installShortcuts() {
	ctrl := gtk.NewShortcutController()
	ctrl.SetScope(gtk.ShortcutScopeGlobal)
	ctrl.SetPropagationPhase(gtk.PhaseCapture)

	addUnconditional := func(spec string, fn func()) {
		t := gtk.NewShortcutTriggerParseString(spec)
		if t == nil {
			slog.Warn("shortcut: unparseable accel", "accel", spec)
			return
		}
		action := gtk.NewCallbackAction(func(_ gtk.Widgetter, _ *glib.Variant) (ok bool) {
			fn()
			return true
		})
		ctrl.AddShortcut(gtk.NewShortcut(t, action))
	}
	// addGated is the propagate-false variant used by clipboard
	// actions: when an editable widget has focus, return false so GTK
	// keeps propagating the event to that widget's native copy/paste
	// handler. Returning true here would swallow it and break paste
	// in the sidebar rename entry.
	addGated := func(spec string, fn func()) {
		t := gtk.NewShortcutTriggerParseString(spec)
		if t == nil {
			slog.Warn("shortcut: unparseable accel", "accel", spec)
			return
		}
		action := gtk.NewCallbackAction(func(_ gtk.Widgetter, _ *glib.Variant) (ok bool) {
			if a.editableHasFocus() {
				return false
			}
			fn()
			return true
		})
		ctrl.AddShortcut(gtk.NewShortcut(t, action))
	}

	type shortcutAction struct {
		fn    func()
		gated bool
	}
	handlers := map[string]shortcutAction{
		ActionNewTab:        {fn: a.newTabInActiveProject},
		ActionCloseTab:      {fn: a.closeActiveTab},
		ActionRenameTab:     {fn: a.renameActiveTab},
		ActionCycleTabPrev:  {fn: func() { a.cycleTab(-1) }},
		ActionCycleTabNext:  {fn: func() { a.cycleTab(1) }},
		ActionPaste:         {fn: a.pasteIntoActive, gated: true},
		ActionCopy:          {fn: a.copyFromActive, gated: true},
		ActionNewProject:    {fn: a.newProject},
		ActionRenameProject: {fn: a.beginRenameActiveProject},
		ActionFontIncrease:  {fn: func() { a.adjustActiveFontSize(+1) }},
		ActionFontDecrease:  {fn: func() { a.adjustActiveFontSize(-1) }},
		ActionFontReset:     {fn: a.resetActiveFontSize},
	}
	for i := 1; i <= 9; i++ {
		i := i
		handlers[switchProjectAction(i)] = shortcutAction{
			fn: func() { a.switchProjectByIndex(i - 1) },
		}
		handlers[switchTabAction(i)] = shortcutAction{
			fn: func() { a.switchTabByIndex(i - 1) },
		}
	}

	known := make(map[string]bool, len(handlers))
	for action := range handlers {
		known[action] = true
	}
	resolved := canonicalizeBindings(
		defaultBindings(), a.cfg.Keybinds, known,
		func(msg, trigger, action string) {
			slog.Warn("shortcut: "+msg, "trigger", trigger, "action", action)
		},
	)

	// Sort the canonical accels before installing so the order is
	// deterministic — matters only if two installable accels collide
	// at the GTK level, but cheap insurance.
	accels := make([]string, 0, len(resolved))
	for accel := range resolved {
		accels = append(accels, accel)
	}
	sort.Strings(accels)

	for _, accel := range accels {
		sa := handlers[resolved[accel]]
		if sa.gated {
			addGated(accel, sa.fn)
		} else {
			addUnconditional(accel, sa.fn)
		}
	}

	a.win.AddController(ctrl)
}

// switchProjectAction / switchTabAction synthesize the indexed action
// names for the project / tab numeric switchers. Kept as small helpers
// so installShortcuts and defaultBindings stay in sync.
func switchProjectAction(i int) string { return "switch_project_" + strconv.Itoa(i) }
func switchTabAction(i int) string     { return "switch_tab_" + strconv.Itoa(i) }

// defaultBindings returns the platform-default trigger list per action,
// in Ghostty trigger syntax. installShortcuts layers user `keybind`
// lines on top via resolveBindings.
//
// Linux clipboardMod is "alt" because the existing default has been
// Alt-V / Alt-C since PR #4; ctrl+shift+v / ctrl+shift+c are kept as
// secondary triggers. macOS clipboardMod is "super".
func defaultBindings() map[string][]string {
	primary := "ctrl"
	projectMod := "alt"
	clipboardMod := "alt"
	if runtime.GOOS == "darwin" {
		primary = "super"
		projectMod = "super"
		clipboardMod = "super"
	}
	m := map[string][]string{
		ActionNewTab:    {primary + "+t"},
		ActionCloseTab:  {primary + "+w"},
		ActionRenameTab: {projectMod + "+r"},
		// Shift-[ produces braceleft on US layouts; bracketleft on
		// layouts that don't transform. Keep both.
		ActionCycleTabPrev: {
			primary + "+shift+braceleft",
			primary + "+shift+bracketleft",
		},
		ActionCycleTabNext: {
			primary + "+shift+braceright",
			primary + "+shift+bracketright",
		},
		ActionPaste:         {clipboardMod + "+v", "ctrl+shift+v"},
		ActionCopy:          {clipboardMod + "+c", "ctrl+shift+c"},
		ActionNewProject:    {projectMod + "+n"},
		ActionRenameProject: {projectMod + "+shift+r"},
		// Browser-style font sizing per tab. + and = both bind because
		// cmd-+ on US layouts is really cmd-shift-=, and many users
		// hit cmd-= without the shift; both work.
		ActionFontIncrease: {primary + "+plus", primary + "+equal"},
		ActionFontDecrease: {primary + "+minus"},
		ActionFontReset:    {primary + "+0"},
	}
	for i := 1; i <= 9; i++ {
		m[switchProjectAction(i)] = []string{projectMod + "+" + strconv.Itoa(i)}
		m[switchTabAction(i)] = []string{"ctrl+" + strconv.Itoa(i)}
	}
	return m
}

// pageKey returns the stable GObject pointer for an AdwTabPage, used
// as the pageTabs map key. gotk4 may return a fresh Go wrapper from
// getter calls (view.SelectedPage, NthPage, etc.) that doesn't
// pointer-equal the wrapper inserted via view.Append, so keying by Go
// pointer would silently miss. The underlying C pointer is stable.
func pageKey(p *adw.TabPage) uintptr {
	if p == nil {
		return 0
	}
	return p.Native()
}

// adjustActiveFontSize / resetActiveFontSize are the cmd+/-/0
// keybinding entry points. They no-op when there is no active tab so
// the empty-window state doesn't flash an error.
func (a *App) adjustActiveFontSize(delta int) {
	if s := a.activeSession(); s != nil {
		s.AdjustFontSize(delta)
	}
}

func (a *App) resetActiveFontSize() {
	if s := a.activeSession(); s != nil {
		s.ResetFontSize()
	}
}

// activeSession returns the currently selected session in the active
// project, or nil if none. Resolves: active project → its TabView's
// selected page → tab id → session.
func (a *App) activeSession() *Session {
	view := a.projectViews[a.activeProjectID]
	if view == nil {
		return nil
	}
	page := view.SelectedPage()
	if page == nil {
		return nil
	}
	id, ok := a.pageTabs[pageKey(page)]
	if !ok {
		return nil
	}
	return a.sessions[id]
}

// editableHasFocus reports whether keyboard focus currently lives in
// a GtkEditable widget — typically a sidebar rename GtkEntry, but
// also any GtkText / GtkTextView a future feature might add. Used
// by the clipboard shortcuts to step aside so the focused entry
// gets its native copy/paste behavior.
//
// We check via type assertion against gtk.EditableTextWidget (the
// gotk4 wrapper for GtkEditable). Walks one parent up to handle the
// AdwEntryRow / GtkSearchEntry case where the actual focusable text
// is a child widget of the visible entry.
func (a *App) editableHasFocus() bool {
	if a.win == nil {
		return false
	}
	w := a.win.Focus()
	for i := 0; i < 2 && w != nil; i++ {
		if _, ok := w.(*gtk.EditableTextWidget); ok {
			return true
		}
		w = gtk.BaseWidget(w).Parent()
	}
	return false
}

// pasteIntoActive drives a clipboard read → encode → background PTY
// write for the active session. Bound to Cmd+V / Alt+V / Ctrl+Shift+V.
//
// The encoded bytes are queued via Session.QueueWrite, which feeds
// them into a per-session buffered channel drained by a single writer
// goroutine. That serialises against keystrokes / mouse events so a
// paste-then-type doesn't interleave on the wire and keeps PTY I/O
// off the GTK main thread.
//
// Limits and behavior:
//   - Cap the clipboard at pasteMaxBytes (4 MiB); above that, log and
//     drop. A confirmation dialog is later polish.
//   - Encode through libghostty-vt's ghostty_paste_encode, which wraps
//     in \x1b[200~ … \x1b[201~ when the foreground app has bracketed
//     paste enabled, strips unsafe control bytes (NUL/ESC/DEL → space,
//     including any embedded \x1b[201~ sentinel that would otherwise
//     let pasted content escape bracketed mode), and replaces \n with
//     \r when not bracketed.
const pasteMaxBytes = 4 * 1024 * 1024

func (a *App) pasteIntoActive() {
	if a.editableHasFocus() {
		return
	}
	sess := a.activeSession()
	if sess == nil {
		return
	}
	display := gdk.DisplayGetDefault()
	if display == nil {
		return
	}
	clip := display.Clipboard()
	clip.ReadTextAsync(context.Background(), func(res gio.AsyncResulter) {
		text, err := clip.ReadTextFinish(res)
		if err != nil {
			slog.Warn("clipboard read", "err", err)
			return
		}
		if text == "" {
			return
		}
		if len(text) > pasteMaxBytes {
			slog.Warn("paste exceeds size limit",
				"bytes", len(text), "limit", pasteMaxBytes)
			return
		}
		bracketed := sess.term.BracketedPasteEnabled()
		encoded, err := ghostty.EncodePaste([]byte(text), bracketed)
		if err != nil {
			slog.Warn("paste encode", "err", err)
			return
		}
		sess.QueueWrite(encoded)
	})
}

// copyFromActive extracts the active session's selection via the
// libghostty-vt formatter (handles soft-wrap unwrap, trailing-space
// trim, interior-whitespace preservation) and places the result on
// the system clipboard. On Linux it also writes to the PRIMARY
// clipboard so middle-click paste in other apps works; PRIMARY
// doesn't exist on macOS and the call is a no-op there.
func (a *App) copyFromActive() {
	if a.editableHasFocus() {
		return
	}
	sess := a.activeSession()
	if sess == nil || sess.sel.empty() {
		return
	}
	sCol, sRow, eCol, eRow := sess.sel.normalized()
	text, err := ghostty.CopyViewportSelection(
		sess.term,
		uint16(sCol), uint32(sRow),
		uint16(eCol), uint32(eRow),
	)
	if err != nil {
		slog.Warn("copy selection", "err", err)
		return
	}
	if text == "" {
		return
	}
	display := gdk.DisplayGetDefault()
	if display == nil {
		return
	}
	display.Clipboard().SetText(text)
	if runtime.GOOS != "darwin" {
		if pc := display.PrimaryClipboard(); pc != nil {
			pc.SetText(text)
		}
	}
}

// emptyStackName is the stack child name for the "no projects" status
// page. Switched in when projectViews becomes empty.
const emptyStackName = "__empty"

// buildEmptyState returns an AdwStatusPage with a "+ Project" button.
func (a *App) buildEmptyState() *adw.StatusPage {
	page := adw.NewStatusPage()
	page.SetIconName("folder-symbolic")
	page.SetTitle("No projects")
	page.SetDescription("Create a project to get started.")

	btn := gtk.NewButtonWithLabel("+ Project")
	btn.AddCSSClass("suggested-action")
	btn.AddCSSClass("pill")
	btn.SetHAlign(gtk.AlignCenter)
	btn.ConnectClicked(func() { a.newProject() })
	page.SetChild(btn)
	return page
}

// beginRenameActiveProject puts the active project's row into edit mode.
// Wired to Cmd-Shift-R on macOS, Alt-Shift-R on Linux; safe no-op when
// there is no active project.
func (a *App) beginRenameActiveProject() {
	if a.activeProjectID == 0 {
		return
	}
	if pr, ok := a.projectRows[a.activeProjectID]; ok {
		pr.enterEditMode()
	}
}

// commitRename applies the entered text. Empty / whitespace-only names
// are silently ignored — the row stays on the previous label.
func (a *App) commitRename(pid int64, text string) {
	name := strings.TrimSpace(text)
	if name == "" {
		return
	}
	pr, ok := a.projectRows[pid]
	if !ok {
		return
	}
	if name == pr.label.Text() {
		return
	}
	if err := a.ws.RenameProject(pid, name); err != nil {
		slog.Error("RenameProject", "pid", pid, "name", name, "err", err)
	}
}

// showRowMenu pops up the Rename / Close menu on the row. The menu
// items reference the per-row action group ("row") that addProjectUI
// installed once at row creation time.
func (a *App) showRowMenu(pr *projectRow) {
	menu := gio.NewMenu()
	menu.AppendItem(gio.NewMenuItem("Rename", "row.rename"))
	menu.AppendItem(gio.NewMenuItem("Close project", "row.close"))

	popover := gtk.NewPopoverMenuFromModel(menu)
	popover.SetParent(pr.row)
	popover.SetHasArrow(false)
	popover.Popup()
}

// requestCloseProject is the entry point for explicit "close this
// project" intents (the hover X, the right-click menu). When the
// project still has tabs we ask first; otherwise we delete immediately.
func (a *App) requestCloseProject(pid int64) {
	view := a.projectViews[pid]
	if view == nil {
		return
	}
	if view.NPages() == 0 {
		a.deleteProject(pid)
		return
	}
	a.showCloseProjectDialog(pid)
}

// showCloseProjectDialog presents the cmux-style confirmation prompt
// before destroying a project that still has open tabs.
func (a *App) showCloseProjectDialog(pid int64) {
	dlg := adw.NewAlertDialog("Close project?", "This will close the project and all of its tabs.")
	dlg.AddResponse("cancel", "Cancel")
	dlg.AddResponse("close", "Close")
	dlg.SetResponseAppearance("close", adw.ResponseDestructive)
	dlg.SetDefaultResponse("cancel")
	dlg.SetCloseResponse("cancel")
	dlg.ConnectResponse(func(resp string) {
		if resp == "close" {
			a.deleteProject(pid)
		}
	})
	dlg.Choose(context.Background(), a.win, nil)
}

// deleteProject tears down a project: frees every Session's PTY and
// libghostty resources, then calls ws.DeleteProject which cascades the
// store delete and emits EventProjectDeleted. UI cleanup (sidebar row,
// stack page removal) lives in removeProjectUI on the event side.
//
// Does NOT call view.ClosePage per tab — that would re-enter the
// close-page handler and possibly call back into deleteProject. The
// stack child is removed wholesale by removeProjectUI, which destroys
// the TabView and all its pages without firing close-page signals.
func (a *App) deleteProject(pid int64) {
	view := a.projectViews[pid]
	if view == nil {
		return
	}
	// Snapshot tab IDs first since we mutate the lookup maps below.
	var tabIDs []int64
	for i := 0; i < int(view.NPages()); i++ {
		page := view.NthPage(i)
		if id, ok := a.pageTabs[pageKey(page)]; ok {
			tabIDs = append(tabIDs, id)
			delete(a.pageTabs, pageKey(page))
			delete(a.tabPages, id)
		}
	}
	for _, id := range tabIDs {
		if sess, ok := a.sessions[id]; ok {
			sess.Close()
			delete(a.sessions, id)
		}
	}
	if err := a.ws.DeleteProject(pid); err != nil {
		slog.Error("DeleteProject", "pid", pid, "err", err)
	}
}

// removeProjectUI tears down the sidebar row and stack page for a
// deleted project. Called from the EventProjectDeleted handler. If
// nothing's left, switches to the empty state and clears the header.
// If the deleted project was active, picks a sensible neighbor.
func (a *App) removeProjectUI(pid int64) {
	if pr, ok := a.projectRows[pid]; ok {
		a.sidebar.Remove(pr.row)
		delete(a.projectRows, pid)
	}
	stackName := strconv.FormatInt(pid, 10)
	if child := a.stack.ChildByName(stackName); child != nil {
		a.stack.Remove(child)
	}
	delete(a.projectViews, pid)

	if a.activeProjectID == pid {
		a.activeProjectID = 0
	}

	if len(a.projectViews) == 0 {
		a.stack.SetVisibleChildName(emptyStackName)
		a.updateHeader()
		// Last project gone → quit. The window's close handler runs
		// shutdown() which cleans up the IPC server + sessions. The
		// empty-state status page above is a defensive fallback in
		// case the close races with anything else.
		a.win.Close()
		return
	}
	if a.activeProjectID == 0 {
		// Pick the top-most remaining sidebar row. Map iteration over
		// projectViews would be non-deterministic; the ListBox order
		// matches what the user sees.
		if row := a.sidebar.RowAtIndex(0); row != nil {
			if id, err := strconv.ParseInt(row.Name(), 10, 64); err == nil {
				a.selectProject(id)
			}
		}
	}
}
