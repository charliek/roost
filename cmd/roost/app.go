package main

import (
	"errors"
	"log/slog"
	"os"
	"strconv"
	"strings"

	"github.com/diamondburned/gotk4-adwaita/pkg/adw"
	coreglib "github.com/diamondburned/gotk4/pkg/core/glib"
	"github.com/diamondburned/gotk4/pkg/glib/v2"
	"github.com/diamondburned/gotk4/pkg/gtk/v4"

	"github.com/charliek/roost/internal/core"
	"github.com/charliek/roost/internal/ipc"
)

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

	// One AdwTabView per project. Stack switches between them so each
	// project keeps its own tab strip + selection state.
	stack        *gtk.Stack
	projectViews map[int64]*adw.TabView

	// The sidebar is a Gtk.ListBox of project rows.
	sidebar     *gtk.ListBox
	projectRows map[int64]*gtk.ListBoxRow

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
		projectRows:  map[int64]*gtk.ListBoxRow{},
		sessions:     map[int64]*Session{},
		pageTabs:     map[*adw.TabPage]int64{},
		tabPages:     map[int64]*adw.TabPage{},
	}
}

// activate is wired to AdwApplication::activate. Builds the entire
// window content and rehydrates persistent state into the UI.
func (a *App) activate() {
	a.win = adw.NewApplicationWindow(&a.gtkApp.Application)
	a.win.SetTitle("Roost")
	a.win.SetDefaultSize(1100, 700)

	// HeaderBar with a "+ tab" button on the right.
	header := adw.NewHeaderBar()
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

	sidebarBox := gtk.NewBox(gtk.OrientationVertical, 0)
	sidebarBox.Append(sidebarScroll)
	sidebarBox.SetVExpand(true)
	sidebarScroll.SetVExpand(true)
	sidebarBox.Append(newProjectBtn)

	// Right side: a Stack of AdwTabView (one per project).
	a.stack = gtk.NewStack()
	a.stack.SetHExpand(true)
	a.stack.SetVExpand(true)

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
	}
}

// addProjectUI creates a sidebar row + a TabView in the stack for one
// project. Idempotent: bails out if the project is already in the UI.
func (a *App) addProjectUI(p core.Project) {
	if _, ok := a.projectViews[p.ID]; ok {
		return
	}
	row := gtk.NewListBoxRow()
	row.SetName(strconv.FormatInt(p.ID, 10))
	label := gtk.NewLabel(p.Name)
	label.SetXAlign(0)
	label.SetMarginTop(8)
	label.SetMarginBottom(8)
	label.SetMarginStart(12)
	label.SetMarginEnd(12)
	row.SetChild(label)
	a.sidebar.Append(row)
	a.projectRows[p.ID] = row

	view := adw.NewTabView()
	view.ConnectClosePage(func(page *adw.TabPage) bool {
		a.closeTab(page)
		view.ClosePageFinish(page, true)
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
	})

	bar := adw.NewTabBar()
	bar.SetView(view)
	bar.SetAutohide(false)

	box := gtk.NewBox(gtk.OrientationVertical, 0)
	box.Append(bar)
	box.Append(view)

	stackName := strconv.FormatInt(p.ID, 10)
	a.stack.AddNamed(box, stackName)
	a.projectViews[p.ID] = view
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
		_ = a.ws.UpdateTabTitle(tabID, title)
	}
	// OSC 7 cwd updates → persist for next-launch shell spawn.
	sess.onPWDChanged = func(cwd string) {
		if cwd == "" {
			return
		}
		_ = a.ws.UpdateTabCWD(tabID, cwd)
	}

	// Clearing the badge when the user actually looks at the tab is
	// done in the project view's selected-page notify handler below.
}

func displayTitle(t core.Tab) string {
	if t.Title != "" {
		return t.Title
	}
	return shorten(t.CWD, 22)
}

func shorten(s string, max int) string {
	if len(s) <= max {
		return s
	}
	return "…" + s[len(s)-max+1:]
}

// selectProject switches the right-side Stack to the given project's
// TabView and updates the activeProjectID.
func (a *App) selectProject(projectID int64) {
	if _, ok := a.projectViews[projectID]; !ok {
		return
	}
	if a.activeProjectID == projectID {
		return
	}
	a.activeProjectID = projectID
	a.stack.SetVisibleChildName(strconv.FormatInt(projectID, 10))

	// Sync sidebar selection (avoids feedback loop because ListBox
	// dedupes selecting an already-selected row).
	if row, ok := a.projectRows[projectID]; ok {
		a.sidebar.SelectRow(row)
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
}

// newProject prompts for a name (auto-named for now) and creates one.
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
}

func (a *App) projectsByName() map[string]bool {
	out := map[string]bool{}
	for id := range a.projectViews {
		row := a.projectRows[id]
		if row != nil {
			if box, ok := row.Child().(*gtk.Label); ok {
				out[box.Label()] = true
			}
		}
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

// closeTab handles AdwTabView::close-page. Frees the Session, deletes
// the tab from the store, and prevents the project from going to zero
// tabs (auto-creates a fresh one if it would).
func (a *App) closeTab(page *adw.TabPage) {
	tabID, ok := a.pageTabs[page]
	if !ok {
		return
	}
	delete(a.pageTabs, page)
	delete(a.tabPages, tabID)

	sess, ok := a.sessions[tabID]
	if ok {
		sess.Close()
		delete(a.sessions, tabID)
	}

	pid := a.activeProjectID
	if sess != nil {
		pid = sess.tab.ProjectID
	}
	if err := a.ws.DeleteTab(tabID); err != nil {
		slog.Error("DeleteTab", "err", err)
	}

	// Don't let the project (or the app) go to zero tabs.
	if view := a.projectViews[pid]; view != nil && view.NPages() == 0 {
		tab, err := a.ws.CreateTab(pid, a.home)
		if err != nil {
			slog.Error("CreateTab fallback", "err", err)
			return
		}
		a.addTabUI(pid, tab)
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

	a.win.AddController(ctrl)
}
