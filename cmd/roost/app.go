package main

import (
	"context"
	_ "embed"
	"errors"
	"log/slog"
	"os"
	"strconv"
	"strings"

	"github.com/diamondburned/gotk4-adwaita/pkg/adw"
	coreglib "github.com/diamondburned/gotk4/pkg/core/glib"
	"github.com/diamondburned/gotk4/pkg/gdk/v4"
	"github.com/diamondburned/gotk4/pkg/gio/v2"
	"github.com/diamondburned/gotk4/pkg/glib/v2"
	"github.com/diamondburned/gotk4/pkg/gtk/v4"

	"github.com/charliek/roost/internal/core"
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
	// page->tab lookup. Each AdwTabPage maps back to a tab ID so we
	// can resolve user actions (close, reorder) to core state.
	pageTabs map[*adw.TabPage]int64
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
		pageTabs:     map[*adw.TabPage]int64{},
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
	newTabBtn.SetTooltipText("New tab in current project (Cmd-T)")
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
	if id, ok := a.pageTabs[page]; ok {
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
	return a.ws.UpdateTabTitle(tabID, title)
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
				if tid, ok := a.pageTabs[page]; ok {
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
		// When a project's last tab closes (whether via UI close, Cmd-W,
		// or a shell exiting), close the project silently. The "are you
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
		if id, ok := a.pageTabs[page]; ok {
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
	a.pageTabs[page] = tab.ID
	a.tabPages[tab.ID] = page

	// OSC 0/1/2 title updates → refresh tab page + persist.
	tabID := tab.ID
	sess.onTitleChanged = func(title string) {
		if title == "" {
			page.SetTitle(displayTitle(sess.tab))
			return
		}
		page.SetTitle(title)
		if err := a.ws.UpdateTabTitle(tabID, title); err != nil {
			slog.Warn("UpdateTabTitle", "tab", tabID, "title", title, "err", err)
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
	id, ok := a.pageTabs[page]
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
		if id, ok := a.pageTabs[page]; ok {
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
			if id, ok := a.pageTabs[page]; ok {
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
	for id, pr := range a.projectRows {
		_ = id
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
	// Inherit the active tab's cwd if we have one.
	if view := a.projectViews[a.activeProjectID]; view != nil {
		if page := view.SelectedPage(); page != nil {
			if id, ok := a.pageTabs[page]; ok {
				if sess, ok := a.sessions[id]; ok {
					cwd = sess.tab.CWD
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
	if tabID, ok := a.pageTabs[page]; ok {
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
	tabID, ok := a.pageTabs[page]
	if !ok {
		return
	}
	delete(a.pageTabs, page)
	delete(a.tabPages, tabID)

	if sess, ok := a.sessions[tabID]; ok {
		sess.Close()
		delete(a.sessions, tabID)
	}

	if err := a.ws.DeleteTab(tabID); err != nil {
		slog.Error("DeleteTab", "err", err)
	}
}

// closeActiveTab is bound to Cmd-W. Closes the currently selected page.
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
// sidebar (Cmd-1..9 mapping).
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

// shutdown closes every Session cleanly. Called on window close.
func (a *App) shutdown() {
	if a.ipcServer != nil {
		_ = a.ipcServer.Close()
	}
	for _, s := range a.sessions {
		s.Close()
	}
}

// installShortcuts wires Ctrl-* shortcuts on the window. The
// ShortcutController is set to PhaseCapture so it runs *before* the
// drawing area's key controller — otherwise terminal-focused keys get
// consumed by handleKey and the shortcut never fires.
//
// Modifier choice: GTK on macOS doesn't reliably deliver Cmd as
// MetaMask, so we bind everything on Ctrl for both platforms (matches
// tmux/screen ergonomics). The <primary> alias resolves to Ctrl on
// Linux and (when it works) Meta on macOS, so we add it as a bonus.
func (a *App) installShortcuts() {
	ctrl := gtk.NewShortcutController()
	ctrl.SetScope(gtk.ShortcutScopeGlobal)
	ctrl.SetPropagationPhase(gtk.PhaseCapture)

	add := func(spec string, fn func()) {
		t := gtk.NewShortcutTriggerParseString(spec)
		if t == nil {
			return
		}
		action := gtk.NewCallbackAction(func(_ gtk.Widgetter, _ *glib.Variant) (ok bool) {
			fn()
			return true
		})
		ctrl.AddShortcut(gtk.NewShortcut(t, action))
	}

	// Bind both <Control> and <primary> so Ctrl works everywhere AND
	// Cmd works on macOS where the platform delivers it.
	bindBoth := func(suffix string, fn func()) {
		add("<Control>"+suffix, fn)
		add("<primary>"+suffix, fn)
	}

	bindBoth("t", a.newTabInActiveProject)
	bindBoth("w", a.closeActiveTab)
	bindBoth("<shift>t", a.newProject)

	// Shift+[ produces braceleft on US layouts. GTK matches the
	// transformed keyval, so we bind the curly forms (and the bracket
	// forms as a safety net for layouts that don't transform).
	for _, k := range []string{"braceleft", "bracketleft"} {
		bindBoth("<shift>"+k, func() { a.cycleTab(-1) })
	}
	for _, k := range []string{"braceright", "bracketright"} {
		bindBoth("<shift>"+k, func() { a.cycleTab(1) })
	}

	for i := 1; i <= 9; i++ {
		idx := i - 1
		bindBoth(strings.TrimLeft(strconv.Itoa(i), "0"), func() {
			a.switchProjectByIndex(idx)
		})
	}

	// F2 enters rename mode on the currently selected sidebar row.
	add("F2", a.beginRenameActiveProject)

	a.win.AddController(ctrl)
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
// Wired to F2; safe no-op when there is no active project.
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
		if id, ok := a.pageTabs[page]; ok {
			tabIDs = append(tabIDs, id)
			delete(a.pageTabs, page)
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
		// Pick the first remaining row.
		for nextID := range a.projectViews {
			a.selectProject(nextID)
			break
		}
	}
}
