//go:build linux

package openuri

import (
	"log/slog"
	"os/exec"
)

// OpenURI opens uri in the user's chosen handler via xdg-open. xdg-open
// reads ~/.config/mimeapps.list and the rest of the XDG MIME Applications
// spec — the canonical "system default" answer that GNOME/COSMIC/KDE
// settings UIs and `xdg-settings set default-web-browser` write to. It is
// also portal-aware inside a Flatpak/Snap sandbox.
//
// We deliberately don't go through xdg-desktop-portal directly. On systems
// where no backend implements the OpenURI interface (notably COSMIC as of
// 2026-05), the portal's built-in handler delegates to GIO's default-app
// database, which can disagree with what xdg-mime reports — landing on a
// different browser than the user configured. xdg-open follows the spec
// literally and matches the user's intent.
func OpenURI(uri string) {
	cmd := exec.Command("xdg-open", uri)
	if err := cmd.Start(); err != nil {
		slog.Warn("xdg-open failed to start", "err", err, "uri", uri)
		return
	}
	// Reap in a detached goroutine to avoid zombies. Result is ignored —
	// xdg-open's own exit status doesn't tell us whether the user's
	// handler actually opened the URL, just whether xdg-open dispatched.
	go func() { _ = cmd.Wait() }()
}
