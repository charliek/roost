package main

import (
	"context"
	_ "embed"
	"errors"
	"log/slog"
	"os"
	"runtime"
	"strconv"
	"strings"

	"github.com/diamondburned/gotk4-adwaita/pkg/adw"
	coreglib "github.com/diamondburned/gotk4/pkg/core/glib"
	"github.com/diamondburned/gotk4/pkg/gdk/v4"
	"github.com/diamondburned/gotk4/pkg/gio/v2"
	"github.com/diamondburned/gotk4/pkg/glib/v2"
	"github.com/diamondburned/gotk4/pkg/gtk/v4"

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

	activeProjectID int64
}

// NewApp wires the app together. The window is built in activate.
func NewApp(gtkApp *adw.Application, ws *core.Workspace, home, socketPath string) *App {
	return &App{
		gtkApp:       gtkApp,
		ws:           ws,
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
		// Already focused — no badge, no desktop notification.
		slog.Info("notification (suppressed; tab active)", "tab", tabID, "title", title)
		return
	}
	page.SetNeedsAttention(true)

	id := "roost.tab." + strconv.FormatInt(tabID, 10)
	sendDesktopNotification(a.gtkApp, id, title, body)
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
		"ROOST_SOCKET=" + a.socketPath,
	}
	sess, err := NewSession(a.ws, tab, initialCols, initialRows, env...)
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

	// Focus the active tab so keystrokes go to the terminal.
	view := a.projectViews[projectID]
	if page := view.SelectedPage(); page != nil {
		if id, ok := a.pageTabs[pageKey(page)]; ok {
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
// Per-platform modifier policy:
//   - macOS: Cmd is the "primary" app modifier (tab management, cycle).
//     Cmd is also the project-management modifier (new project, rename
//     project, rename tab, switch project 1..9). Ctrl-1..9 switches
//     tabs in the active project.
//   - Linux: Ctrl is the primary app modifier. Alt is the project-
//     management modifier (mirrors Cmd on macOS). Ctrl-1..9 switches
//     tabs.
//
// On macOS gdk-macos translates NSEventModifierFlagCommand directly to
// GDK_META_MASK, so <Meta>x in a trigger reliably matches Cmd-x. The
// <Primary> alias is hardcoded to Control on every platform in GTK4, so
// we substitute the modifier ourselves rather than relying on it.
func (a *App) installShortcuts() {
	ctrl := gtk.NewShortcutController()
	ctrl.SetScope(gtk.ShortcutScopeGlobal)
	ctrl.SetPropagationPhase(gtk.PhaseCapture)

	add := func(spec string, fn func()) {
		t := gtk.NewShortcutTriggerParseString(spec)
		if t == nil {
			slog.Warn("shortcut: trigger parse failed", "spec", spec)
			return
		}
		action := gtk.NewCallbackAction(func(_ gtk.Widgetter, _ *glib.Variant) (ok bool) {
			fn()
			return true
		})
		ctrl.AddShortcut(gtk.NewShortcut(t, action))
	}

	// addCond is like add but lets the action signal "I didn't
	// handle this; let GTK keep propagating." Used for clipboard
	// shortcuts so a focused GtkEditable can keep its native
	// copy/paste behavior — returning false here lets GTK deliver
	// the keystroke to the focused widget after our capture-phase
	// controller declines.
	addCond := func(spec string, fn func() bool) {
		t := gtk.NewShortcutTriggerParseString(spec)
		if t == nil {
			slog.Warn("shortcut: trigger parse failed", "spec", spec)
			return
		}
		action := gtk.NewCallbackAction(func(_ gtk.Widgetter, _ *glib.Variant) (ok bool) {
			return fn()
		})
		ctrl.AddShortcut(gtk.NewShortcut(t, action))
	}

	primary := "<Control>"
	projectMod := "<Alt>"
	if runtime.GOOS == "darwin" {
		primary = "<Meta>"
		projectMod = "<Meta>"
	}

	// Tab management on the primary modifier.
	add(primary+"t", a.newTabInActiveProject)
	add(primary+"w", a.closeActiveTab)

	// Clipboard. Cmd+V on macOS, Alt+V on Linux, Ctrl+Shift+V on both
	// (terminal convention) so muscle memory works either way. Bare
	// Ctrl+V remains as terminal input. Bare Ctrl+C is left as SIGINT
	// — Cmd+C / Alt+C / Ctrl+Shift+C handle copy without overloading
	// the SIGINT key.
	clipboardMod := "<Alt>"
	if runtime.GOOS == "darwin" {
		clipboardMod = "<Meta>"
	}
	clipboardGuard := func(fn func()) func() bool {
		return func() bool {
			if a.editableHasFocus() {
				// Tell GTK we didn't consume the event so the
				// focused entry / text view gets its native
				// copy/paste handling.
				return false
			}
			fn()
			return true
		}
	}
	addCond(clipboardMod+"v", clipboardGuard(a.pasteIntoActive))
	addCond("<Control><Shift>v", clipboardGuard(a.pasteIntoActive))
	addCond(clipboardMod+"c", clipboardGuard(a.copyFromActive))
	addCond("<Control><Shift>c", clipboardGuard(a.copyFromActive))

	// Shift+[ produces braceleft on US layouts. GTK matches the
	// transformed keyval, so we bind the curly forms (and the bracket
	// forms as a safety net for layouts that don't transform).
	for _, k := range []string{"braceleft", "bracketleft"} {
		add(primary+"<Shift>"+k, func() { a.cycleTab(-1) })
	}
	for _, k := range []string{"braceright", "bracketright"} {
		add(primary+"<Shift>"+k, func() { a.cycleTab(1) })
	}

	// Project / tab management on the project modifier.
	add(projectMod+"n", a.newProject)
	add(projectMod+"<Shift>r", a.beginRenameActiveProject)
	add(projectMod+"r", a.renameActiveTab)

	// Numeric switchers: project on the project modifier, tab on Ctrl.
	for i := 1; i <= 9; i++ {
		idx := i - 1
		n := strconv.Itoa(i)
		add(projectMod+n, func() { a.switchProjectByIndex(idx) })
		add("<Control>"+n, func() { a.switchTabByIndex(idx) })
	}

	a.win.AddController(ctrl)
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
// The encoded bytes are queued via Session.QueueWrite, which runs the
// actual pty.Write on a per-tab goroutine so a slow PTY consumer
// can't stall the GTK main thread. Per-session writeMu serializes
// against keystrokes / mouse events so a paste-then-type doesn't
// interleave on the wire.
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
