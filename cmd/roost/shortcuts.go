package main

import (
	"strings"

	"github.com/charliek/roost/internal/config"
)

// Action names. Matches the snake_case strings users write in
// `keybind = trigger=<action>` lines. New shortcuts add an entry here,
// in the defaultBindings table, and in the install-loop handler map.
const (
	ActionNewTab        = "new_tab"
	ActionCloseTab      = "close_tab"
	ActionRenameTab     = "rename_tab"
	ActionCycleTabPrev  = "cycle_tab_prev"
	ActionCycleTabNext  = "cycle_tab_next"
	ActionPaste         = "paste"
	ActionCopy          = "copy"
	ActionNewProject    = "new_project"
	ActionRenameProject = "rename_project"
	// switch_project_1..9 and switch_tab_1..9 are generated; not const'd.
	ActionUnbind = "unbind"
)

// triggerToAccel converts a Ghostty-style trigger ("super+shift+t") to
// a GTK accelerator string ("<Meta><Shift>t"). Returns ok=false on
// unknown modifiers, an empty key, or empty input; the caller is
// expected to slog.Warn and skip.
//
// Modifier aliases match Ghostty: ctrl/control, alt/opt/option,
// super/cmd/command. The key segment (last) passes through unchanged
// — gtk.NewShortcutTriggerParseString handles the keyval lookup.
func triggerToAccel(trigger string) (string, bool) {
	parts := strings.Split(strings.TrimSpace(trigger), "+")
	if len(parts) == 0 {
		return "", false
	}
	key := strings.TrimSpace(parts[len(parts)-1])
	if key == "" {
		return "", false
	}
	var b strings.Builder
	for _, m := range parts[:len(parts)-1] {
		switch strings.ToLower(strings.TrimSpace(m)) {
		case "shift":
			b.WriteString("<Shift>")
		case "ctrl", "control":
			b.WriteString("<Control>")
		case "alt", "opt", "option":
			b.WriteString("<Alt>")
		case "super", "cmd", "command":
			b.WriteString("<Meta>")
		default:
			return "", false
		}
	}
	b.WriteString(key)
	return b.String(), true
}

// resolveBindings layers user keybinds on top of the platform defaults
// and returns a map from Ghostty trigger → action name.
//
// Semantics (matches Ghostty):
//   - Defaults seed the map.
//   - Each user keybind, in source order, sets or removes one trigger:
//     `unbind` deletes; any other action assigns the trigger.
//   - Last write wins per trigger.
//   - Removing the only trigger of an action makes that action
//     unreachable (intentional).
//
// Pure: no GTK calls, deterministic output. Unit-tested in isolation.
func resolveBindings(defaults map[string][]string, user []config.Keybind) map[string]string {
	triggerToAction := make(map[string]string, len(defaults))
	for action, triggers := range defaults {
		for _, t := range triggers {
			triggerToAction[t] = action
		}
	}
	for _, kb := range user {
		if kb.Action == ActionUnbind {
			delete(triggerToAction, kb.Trigger)
			continue
		}
		triggerToAction[kb.Trigger] = kb.Action
	}
	return triggerToAction
}
