package main

import (
	_ "embed"

	"github.com/diamondburned/gotk4/pkg/gio/v2"
	"github.com/diamondburned/gotk4/pkg/glib/v2"

	"github.com/charliek/roost/internal/core"
)

// Three small SVG circles, one per visible agent state. Embedded as
// bytes and wrapped in gio.BytesIcon so AdwTabPage's indicator slot
// can render them. Hard-coded fills (no libadwaita theming): the
// indicator icons are semantic status colors, not chrome.
//
// gio.ThemedIcon recolors based on the parent widget's CSS, which
// would force every tab's indicator to share one color — useless for
// per-tab divergent state. BytesIcon paints the SVG as-is.

//go:embed icon_running.svg
var iconRunningBytes []byte

//go:embed icon_needs_input.svg
var iconNeedsInputBytes []byte

//go:embed icon_idle.svg
var iconIdleBytes []byte

// stateIcons is built once at App.activate() — gio.BytesIcon is
// reference-counted on the Go side, but constructing them per
// tab-update would still be wasteful.
type stateIcons struct {
	running    *gio.BytesIcon
	needsInput *gio.BytesIcon
	idle       *gio.BytesIcon
}

func newStateIcons() stateIcons {
	mk := func(b []byte) *gio.BytesIcon {
		return gio.NewBytesIcon(glib.NewBytes(b))
	}
	return stateIcons{
		running:    mk(iconRunningBytes),
		needsInput: mk(iconNeedsInputBytes),
		idle:       mk(iconIdleBytes),
	}
}

// indicatorIconForState maps a state to its cached icon. Returns nil
// for TabAgentNone (clears the indicator slot).
func (a *App) indicatorIconForState(s core.TabAgentState) gio.Iconner {
	switch s {
	case core.TabAgentRunning:
		return a.stateIcons.running
	case core.TabAgentNeedsInput:
		return a.stateIcons.needsInput
	case core.TabAgentIdle:
		return a.stateIcons.idle
	}
	return nil
}
