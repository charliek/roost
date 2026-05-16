package main

import (
	_ "embed"

	"github.com/diamondburned/gotk4/pkg/gio/v2"
	"github.com/diamondburned/gotk4/pkg/glib/v2"
)

//go:generate glib-compile-resources --target=icons/icons.gresource --sourcedir=icons icons/icons.gresource.xml

//go:embed icons/icons.gresource
var iconsResourceData []byte

const iconResourcePrefix = "/dev/charliek/roost/icons"

// init registers the embedded icon bundle with GLib's resource system
// so the AddResourcePath call in app.activate can resolve the names
// we use (folder-symbolic, sidebar-show-symbolic, etc.). Bundling
// avoids depending on the Adwaita icon theme via XDG_DATA_DIRS, which
// launchers like cmux and Finder strip down so far that GTK can't find
// /opt/homebrew/share/icons.
func init() {
	res, err := gio.NewResourceFromData(glib.NewBytesWithGo(iconsResourceData))
	if err != nil {
		panic("roost: failed to load bundled icon resource: " + err.Error())
	}
	gio.ResourcesRegister(res)
}
