//go:build darwin

package openuri

import (
	"context"

	"github.com/diamondburned/gotk4/pkg/gtk/v4"
)

// OpenURI dispatches uri through gtk.URILauncher, which on macOS goes
// to GIO and ultimately to Launch Services — the system's canonical
// default-handler resolver. No portal involvement on macOS.
func OpenURI(uri string) {
	gtk.NewURILauncher(uri).Launch(context.Background(), nil, nil)
}
