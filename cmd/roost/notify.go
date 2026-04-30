package main

import (
	"fmt"
	"log/slog"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strconv"

	"github.com/diamondburned/gotk4-adwaita/pkg/adw"
	"github.com/diamondburned/gotk4/pkg/gio/v2"
	"github.com/diamondburned/gotk4/pkg/glib/v2"
)

// sendDesktopNotification surfaces a desktop notification for the given
// tab. Both platforms support click-through: clicking the banner
// invokes the app-level "tab-focus" GIO action (Linux) or shells out
// to `roost-cli tab focus --tab N` (macOS via terminal-notifier),
// which raises the window and selects the tab.
//
// macOS: terminal-notifier is required. If absent, we log a one-time
// warning at startup (notifierLookup) and the desktop banner becomes
// a no-op for the rest of the session — in-app indicators continue
// working. Distribution declares it as a Homebrew dependency.
//
// Linux: gio.Notification → freedesktop notification daemon over
// DBus, with a default action wired to tab-focus. Most modern
// notification daemons (mako, dunst, GNOME Shell) honor the action.
func sendDesktopNotification(app *adw.Application, id string, tabID int64, title, body, notifier, cliPath string) {
	if runtime.GOOS == "darwin" {
		sendMacNotification(notifier, cliPath, tabID, title, body)
		return
	}
	n := gio.NewNotification(title)
	if body != "" {
		n.SetBody(body)
	}
	// Default action: clicking the banner activates app.tab-focus
	// with the tab id as parameter. Set up by App.activate via
	// gio.NewSimpleAction.
	n.SetDefaultActionAndTarget("app.tab-focus", glib.NewVariantInt64(tabID))
	app.SendNotification(id, n)
}

// sendMacNotification shells out to terminal-notifier. Required for
// click-through on macOS — osascript can't carry an actionable
// callback. Branding is "terminal-notifier" rather than "Roost"
// because we don't yet ship as a code-signed .app bundle; that's
// Layer 3, separate work.
//
// `-group` lets a fresh notification supersede a prior one for the
// same tab (best-effort on Sonoma+ with unsigned senders).
// `-execute` runs the focus command on click. Args go through argv
// (no shell) so most fields don't need quoting; -execute is shell-
// parsed by terminal-notifier itself, so the cli path needs the
// single-quote escaping treatment.
//
// We Start the child and Wait on a goroutine so the kernel reaps it.
func sendMacNotification(notifier, cliPath string, tabID int64, title, body string) {
	if notifier == "" {
		// Logged once at startup; skip the per-call noise.
		return
	}
	group := "roost.tab." + strconv.FormatInt(tabID, 10)
	args := []string{
		"-title", title,
		"-message", body,
		"-group", group,
	}
	// Click-through requires roost-cli on disk. Without it the banner
	// still fires (in-app indicators are the primary surface anyway);
	// we just skip -execute rather than passing a malformed empty
	// command.
	if cliPath != "" {
		execCmd := fmt.Sprintf("%s tab focus --tab %d", quoteForExecute(cliPath), tabID)
		args = append(args, "-execute", execCmd)
	}
	cmd := exec.Command(notifier, args...)
	if err := cmd.Start(); err != nil {
		slog.Warn("terminal-notifier start", "err", err)
		return
	}
	go func() { _ = cmd.Wait() }()
}

// quoteForExecute single-quotes a path if it contains shell-meaningful
// characters. terminal-notifier's -execute is shell-parsed, so a path
// with spaces (rare on macOS but possible in $HOME) would otherwise
// split into two tokens.
func quoteForExecute(s string) string {
	for _, c := range s {
		if c == ' ' || c == '\t' || c == '"' || c == '$' || c == '\\' || c == '`' || c == '\'' {
			out := []byte{'\''}
			for i := 0; i < len(s); i++ {
				if s[i] == '\'' {
					out = append(out, []byte(`'\''`)...)
				} else {
					out = append(out, s[i])
				}
			}
			out = append(out, '\'')
			return string(out)
		}
	}
	return s
}

// lookupTerminalNotifier resolves the absolute path to terminal-notifier
// once at App startup. Empty string means "not installed; skip macOS
// banners." Logged via the caller.
func lookupTerminalNotifier() string {
	p, err := exec.LookPath("terminal-notifier")
	if err != nil {
		return ""
	}
	return p
}

// lookupRoostCLI resolves the absolute path to roost-cli for the
// click-through target. Tries the directory of the running roost
// binary first (the shipped layout puts the two side-by-side), then
// falls back to PATH. Empty string means click-through is unavailable
// — the banner still fires, just isn't actionable.
func lookupRoostCLI() string {
	if exe, err := os.Executable(); err == nil {
		if abs, err := filepath.Abs(exe); err == nil {
			candidate := filepath.Join(filepath.Dir(abs), "roost-cli")
			if info, err := os.Stat(candidate); err == nil && !info.IsDir() {
				return candidate
			}
		}
	}
	if p, err := exec.LookPath("roost-cli"); err == nil {
		return p
	}
	return ""
}
