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
// and returns a map from Ghostty trigger → action name. Pure structural
// merge: literal trigger strings as keys, no validation. Production no
// longer calls this — installShortcuts uses canonicalizeBindings, which
// collapses aliases and validates user entries. Kept for the tests
// that pin the structural-merge contract.
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

// canonicalizeBindings is the production merge. Returns a map from
// canonical GTK accel → action, with three properties the literal-key
// form lacks:
//
//  1. Alias collapse: super+t / cmd+t / command+t all canonicalize to
//     <Meta>t, so `keybind = cmd+t = unbind` correctly removes a macOS
//     default seeded as `super+t`.
//  2. Action validation: a user keybind whose action isn't in
//     knownActions is reported via warn and skipped — the default
//     binding for that trigger is preserved (a typo can't erase the
//     default).
//  3. Trigger validation: unparseable user triggers are reported and
//     skipped; unparseable default triggers (a bug in defaultBindings)
//     are reported but skipped so the rest of the table still installs.
//
// Pure: no GTK calls. The warn callback lets the production caller
// emit slog and lets tests capture without setup.
func canonicalizeBindings(
	defaults map[string][]string,
	user []config.Keybind,
	knownActions map[string]bool,
	warn func(msg, trigger, action string),
) map[string]string {
	accelToAction := map[string]string{}
	for action, triggers := range defaults {
		for _, t := range triggers {
			accel, ok := triggerToAccel(t)
			if !ok {
				warn("unparseable default trigger", t, action)
				continue
			}
			accelToAction[accel] = action
		}
	}
	for _, kb := range user {
		accel, ok := triggerToAccel(kb.Trigger)
		if !ok {
			warn("unparseable trigger (default kept)", kb.Trigger, kb.Action)
			continue
		}
		if kb.Action == ActionUnbind {
			delete(accelToAction, accel)
			continue
		}
		if !knownActions[kb.Action] {
			warn("unknown action (default kept)", kb.Trigger, kb.Action)
			continue
		}
		accelToAction[accel] = kb.Action
	}
	return accelToAction
}
