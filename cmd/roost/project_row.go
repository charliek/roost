package main

import (
	"github.com/diamondburned/gotk4/pkg/gdk/v4"
	"github.com/diamondburned/gotk4/pkg/gtk/v4"
	"github.com/diamondburned/gotk4/pkg/pango"
)

// projectRow bundles the per-row sidebar widgets for a project. The row
// hosts a label/entry stack so the name can be edited inline, and a
// hover-revealed close button on the right.
type projectRow struct {
	row      *gtk.ListBoxRow
	stack    *gtk.Stack
	label    *gtk.Label
	entry    *gtk.Entry
	revealer *gtk.Revealer
	closeBtn *gtk.Button

	// editing is true while the inline GtkEntry is showing. cancel is
	// set briefly inside the Escape handler so the focus-out callback
	// can tell "Escape pressed" from "user clicked elsewhere".
	editing bool
	cancel  bool
}

const (
	rowStackLabel = "label"
	rowStackEntry = "entry"
)

// newProjectRow builds the row widget tree. Callers wire interaction
// behavior (rename commit, close click) by setting the methods on the
// returned struct or attaching directly to its widgets.
func newProjectRow(name string) *projectRow {
	pr := &projectRow{}

	pr.label = gtk.NewLabel(name)
	pr.label.SetXAlign(0)
	pr.label.SetEllipsize(pango.EllipsizeEnd)

	pr.entry = gtk.NewEntry()
	pr.entry.SetText(name)

	pr.stack = gtk.NewStack()
	pr.stack.AddNamed(pr.label, rowStackLabel)
	pr.stack.AddNamed(pr.entry, rowStackEntry)
	pr.stack.SetVisibleChildName(rowStackLabel)
	pr.stack.SetHExpand(true)

	pr.closeBtn = gtk.NewButtonFromIconName("window-close-symbolic")
	pr.closeBtn.AddCSSClass("flat")
	pr.closeBtn.AddCSSClass("circular")
	pr.closeBtn.SetTooltipText("Close project")

	pr.revealer = gtk.NewRevealer()
	pr.revealer.SetTransitionType(gtk.RevealerTransitionTypeCrossfade)
	pr.revealer.SetTransitionDuration(120)
	pr.revealer.SetChild(pr.closeBtn)
	pr.revealer.SetRevealChild(false)

	box := gtk.NewBox(gtk.OrientationHorizontal, 6)
	box.SetMarginTop(6)
	box.SetMarginBottom(6)
	box.SetMarginStart(12)
	box.SetMarginEnd(8)
	box.Append(pr.stack)
	box.Append(pr.revealer)

	pr.row = gtk.NewListBoxRow()
	pr.row.SetChild(box)

	// Reveal the close button on hover or when the row gets keyboard focus.
	motion := gtk.NewEventControllerMotion()
	motion.ConnectEnter(func(_, _ float64) { pr.revealer.SetRevealChild(true) })
	motion.ConnectLeave(func() {
		if !pr.row.HasFocus() {
			pr.revealer.SetRevealChild(false)
		}
	})
	pr.row.AddController(motion)

	focus := gtk.NewEventControllerFocus()
	focus.ConnectEnter(func() { pr.revealer.SetRevealChild(true) })
	focus.ConnectLeave(func() { pr.revealer.SetRevealChild(false) })
	pr.row.AddController(focus)

	return pr
}

// enterEditMode swaps the row to the GtkEntry and selects all text so
// typing replaces the existing name.
func (pr *projectRow) enterEditMode() {
	if pr.editing {
		return
	}
	pr.editing = true
	pr.entry.SetText(pr.label.Text())
	pr.stack.SetVisibleChildName(rowStackEntry)
	pr.entry.GrabFocus()
	pr.entry.SelectRegion(0, -1)
}

// exitEditMode swaps back to the label. Returns the raw text the user
// typed (callers are expected to trim/validate) and a bool indicating
// whether the change should be committed (false on cancel).
func (pr *projectRow) exitEditMode(commit bool) (text string, ok bool) {
	if !pr.editing {
		return "", false
	}
	pr.editing = false
	text = pr.entry.Text()
	pr.stack.SetVisibleChildName(rowStackLabel)
	return text, commit
}

// setName updates the label (the source-of-truth display string) and
// also resets the entry's text so the next edit starts from the new
// name rather than a stale draft.
func (pr *projectRow) setName(name string) {
	pr.label.SetText(name)
	pr.entry.SetText(name)
}

// installEditControllers wires F2/Escape on the entry. Caller provides
// the commit callback (called from Enter/focus-out) and the cancel
// callback (called from Escape).
func (pr *projectRow) installEditControllers(onCommit func(string), onCancel func()) {
	pr.entry.ConnectActivate(func() {
		text, _ := pr.exitEditMode(true)
		onCommit(text)
	})

	keyCtrl := gtk.NewEventControllerKey()
	keyCtrl.ConnectKeyPressed(func(keyval, _ uint, _ gdk.ModifierType) bool {
		if keyval == gdk.KEY_Escape {
			pr.cancel = true
			pr.exitEditMode(false)
			onCancel()
			pr.cancel = false
			return true
		}
		return false
	})
	pr.entry.AddController(keyCtrl)

	focus := gtk.NewEventControllerFocus()
	focus.ConnectLeave(func() {
		if pr.cancel || !pr.editing {
			return
		}
		text, _ := pr.exitEditMode(true)
		onCommit(text)
	})
	pr.entry.AddController(focus)
}
