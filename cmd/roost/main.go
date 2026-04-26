// Command roost is the GUI binary. It opens a single window with a
// project sidebar on the left and a tab strip per active project on
// the right. Each tab hosts a libghostty-vt terminal driven by its
// own PTY.
package main

import (
	"log"
	"os"

	"github.com/diamondburned/gotk4-adwaita/pkg/adw"

	"github.com/charliek/roost/internal/config"
	"github.com/charliek/roost/internal/core"
	"github.com/charliek/roost/internal/store"
)

const (
	initialCols = 80
	initialRows = 24
	fontFamily  = "Monaco"
	fontSizePt  = 12
	pad         = 4
)

func main() {
	installLogFilter()

	paths, err := config.Resolve()
	if err != nil {
		log.Fatalf("config.Resolve: %v", err)
	}
	if err := paths.EnsureDirs(); err != nil {
		log.Fatalf("config.EnsureDirs: %v", err)
	}

	st, err := store.Open(paths.DBPath())
	if err != nil {
		log.Fatalf("store.Open(%s): %v", paths.DBPath(), err)
	}
	defer st.Close()

	ws := core.New(st)
	home, err := os.UserHomeDir()
	if err != nil {
		log.Fatalf("os.UserHomeDir: %v", err)
	}
	if _, _, err := ws.EnsureDefault(home); err != nil {
		log.Fatalf("EnsureDefault: %v", err)
	}

	gtkApp := adw.NewApplication("dev.charliek.roost", 0)
	app := NewApp(gtkApp, ws, home, paths.SocketPath())
	gtkApp.ConnectActivate(app.activate)
	if code := gtkApp.Run(os.Args); code > 0 {
		log.Fatalf("roost exited with code %d", code)
	}
}
