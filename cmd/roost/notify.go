package main

import (
	"log/slog"
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
	app.SendNotification(id, n)
}

// sendMacNotification shells out to osascript. Inputs are escaped for
// AppleScript string literal safety (backslash + double-quote). osascript
// startup is ~50ms; debounce upstream if we ever fire many in a row.
//
// We Start the child and Wait on a goroutine so the kernel reaps the
// process — without this, every notification leaks a zombie until the
// parent exits.
func sendMacNotification(title, body string) {
	script := `display notification "` + escapeApplescript(body) +
		`" with title "` + escapeApplescript(title) + `"`
	cmd := exec.Command("osascript", "-e", script)
	if err := cmd.Start(); err != nil {
		slog.Warn("osascript start", "err", err)
		return
	}
	go func() { _ = cmd.Wait() }()
}

// escapeApplescript escapes a string for embedding inside an
// AppleScript double-quoted literal. Newlines must be turned into
// `\n` because a raw newline closes nothing in AppleScript but does
// break the surrounding `display notification "..."` invocation.
func escapeApplescript(s string) string {
	s = strings.ReplaceAll(s, `\`, `\\`)
	s = strings.ReplaceAll(s, `"`, `\"`)
	s = strings.ReplaceAll(s, "\n", `\n`)
	s = strings.ReplaceAll(s, "\r", `\r`)
	return s
}
