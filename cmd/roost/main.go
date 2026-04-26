package main

import (
	"log"
	"os"

	"github.com/diamondburned/gotk4-adwaita/pkg/adw"
	"github.com/diamondburned/gotk4/pkg/cairo"
	"github.com/diamondburned/gotk4/pkg/gtk/v4"
)

// Phase 0 spike: open a window with a GtkDrawingArea filling it. The drawing
// area is the surface that will eventually host one libghostty-vt terminal.
// Right now it just fills the canvas with the terminal-bg-ish color so we
// can confirm gotk4, GTK4, libadwaita, Cairo, and the build link cleanly.
func main() {
	app := adw.NewApplication("dev.charliek.roost", 0)
	app.ConnectActivate(func() { activate(app) })
	if code := app.Run(os.Args); code > 0 {
		log.Fatalf("roost exited with code %d", code)
	}
}

func activate(app *adw.Application) {
	win := adw.NewApplicationWindow(&app.Application)
	win.SetTitle("Roost")
	win.SetDefaultSize(900, 600)

	header := adw.NewHeaderBar()

	surface := gtk.NewDrawingArea()
	surface.SetHExpand(true)
	surface.SetVExpand(true)
	surface.SetDrawFunc(func(_ *gtk.DrawingArea, cr *cairo.Context, w, h int) {
		// Background fill — eventually the terminal default bg.
		cr.SetSourceRGB(0.07, 0.08, 0.10)
		cr.Paint()

		// Placeholder: a row of cell-sized rects so we can see the cell
		// alignment math is correct before we hook up libghostty-vt.
		cr.SetSourceRGB(0.20, 0.22, 0.25)
		cellW, cellH := 9, 18
		for x := 8; x+cellW < w; x += cellW + 2 {
			cr.Rectangle(float64(x), 8, float64(cellW), float64(cellH))
		}
		cr.Fill()
	})

	box := gtk.NewBox(gtk.OrientationVertical, 0)
	box.Append(header)
	box.Append(surface)
	win.SetContent(box)

	win.Present()
}
