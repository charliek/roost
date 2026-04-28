// Command roost is the GUI binary. It opens a single window with a
// project sidebar on the left and a tab strip per active project on
// the right. Each tab hosts a libghostty-vt terminal driven by its
// own PTY.
package main

import (
	"errors"
	"fmt"
	"io/fs"
	"log"
	"log/slog"
	"os"

	"github.com/diamondburned/gotk4-adwaita/pkg/adw"

	"github.com/charliek/roost/internal/config"
	"github.com/charliek/roost/internal/core"
	"github.com/charliek/roost/internal/store"
)

const (
	initialCols = 80
	initialRows = 24
	pad         = 8
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
	cfg, err := paths.Load()
	if err != nil {
		log.Fatalf("config.Load: %v", err)
	}
	warnLegacyMacConfig(paths)

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
	app := NewApp(gtkApp, ws, cfg, home, paths.SocketPath())
	gtkApp.ConnectActivate(app.activate)
	if code := gtkApp.Run(os.Args); code > 0 {
		log.Fatalf("roost exited with code %d", code)
	}
}

// warnLegacyMacConfig logs a one-shot migration hint when the user has
// a pre-cutover ~/Library/Application Support/Roost/config.toml but no
// new ~/.config/roost/config.conf. No automatic migration; the move is
// trivial and we don't want to silently rewrite a user's edited file.
func warnLegacyMacConfig(p config.Paths) {
	legacy := p.LegacyMacConfigFile()
	if _, err := os.Stat(legacy); err != nil {
		return
	}
	if _, err := os.Stat(p.ConfigFile()); err == nil {
		return // user has both; assume they migrated
	} else if !errors.Is(err, fs.ErrNotExist) {
		return
	}
	// %q quotes both paths so the macOS path with spaces (Library/
	// Application Support/Roost/...) is copy-paste safe.
	slog.Warn("legacy config detected; not auto-migrating",
		"old", legacy,
		"new", p.ConfigFile(),
		"hint", fmt.Sprintf("mv %q %q", legacy, p.ConfigFile()))
}
