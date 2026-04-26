package main

import (
	"os/exec"
	"runtime"
	"strings"

	"github.com/diamondburned/gotk4-adwaita/pkg/adw"
	"github.com/diamondburned/gotk4/pkg/gio/v2"
)

// sendDesktopNotification surfaces a desktop notification for the given
// tab. Platform-specific routing:
//
//   - Linux: gio.Notification → freedesktop notification daemon over DBus.
//   - macOS: osascript fallback. GIO on Mac tries DBus and silently
//     fails (no DBus session bus), so the gio path is a dead end here.
//
// id is the GApplication notification id; on Linux this lets a second
// notification on the same tab supersede the first instead of stacking.
func sendDesktopNotification(app *adw.Application, id, title, body string) {
	if runtime.GOOS == "darwin" {
		sendMacNotification(title, body)
		return
	}
	n := gio.NewNotification(title)
	if body != "" {
		n.SetBody(body)
	}
	app.Application.SendNotification(id, n)
}

// sendMacNotification shells out to osascript. Inputs are escaped for
// AppleScript string literal safety (backslash + double-quote). osascript
// startup is ~50ms; debounce upstream if we ever fire many in a row.
func sendMacNotification(title, body string) {
	script := `display notification "` + escapeApplescript(body) +
		`" with title "` + escapeApplescript(title) + `"`
	_ = exec.Command("osascript", "-e", script).Start()
}

func escapeApplescript(s string) string {
	s = strings.ReplaceAll(s, `\`, `\\`)
	s = strings.ReplaceAll(s, `"`, `\"`)
	return s
}
