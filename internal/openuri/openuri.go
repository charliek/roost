// Package openuri opens a URL in the user's chosen handler.
//
// On Linux, OpenURI runs `xdg-open` — the canonical XDG MIME Applications
// dispatcher. xdg-open honors ~/.config/mimeapps.list (what
// `xdg-settings set default-web-browser` writes and what GNOME/COSMIC/KDE
// settings UIs configure), and is portal-aware inside Flatpak/Snap sandboxes.
//
// On macOS, OpenURI uses GTK's URILauncher, which dispatches through Launch
// Services (the macOS canonical default-handler resolver).
//
// Per-OS implementations live in openuri_linux.go and openuri_darwin.go.
package openuri
