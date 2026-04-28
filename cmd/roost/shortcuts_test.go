package main

import (
	"sort"
	"testing"

	"github.com/charliek/roost/internal/config"
)

func TestTriggerToAccelAliases(t *testing.T) {
	cases := map[string]string{
		"super+t":           "<Meta>t",
		"cmd+t":             "<Meta>t",
		"command+t":         "<Meta>t",
		"ctrl+t":            "<Control>t",
		"control+t":         "<Control>t",
		"alt+v":             "<Alt>v",
		"opt+v":             "<Alt>v",
		"option+v":          "<Alt>v",
		"shift+Tab":         "<Shift>Tab",
		"super+shift+t":     "<Meta><Shift>t",
		"Super+T":           "<Meta>T",
		"SUPER+t":           "<Meta>t",
		"ctrl+shift+v":      "<Control><Shift>v",
		"ctrl+shift+1":      "<Control><Shift>1",
		"super+bracketleft": "<Meta>bracketleft",
	}
	for in, want := range cases {
		got, ok := triggerToAccel(in)
		if !ok {
			t.Errorf("triggerToAccel(%q) ok=false, want %q", in, want)
			continue
		}
		if got != want {
			t.Errorf("triggerToAccel(%q): got %q want %q", in, got, want)
		}
	}
}

func TestTriggerToAccelRejects(t *testing.T) {
	cases := []string{
		"hyper+t", // unknown modifier
		"super+",  // empty key
		"",        // empty input
		"+t",      // empty modifier slot
		"   ",     // whitespace only
	}
	for _, in := range cases {
		if got, ok := triggerToAccel(in); ok {
			t.Errorf("triggerToAccel(%q) accepted as %q, expected reject", in, got)
		}
	}
}

func TestResolveBindingsEmptyUser(t *testing.T) {
	defaults := map[string][]string{
		"new_tab":   {"super+t"},
		"close_tab": {"super+w"},
	}
	got := resolveBindings(defaults, nil)
	want := map[string]string{
		"super+t": "new_tab",
		"super+w": "close_tab",
	}
	if !mapEq(got, want) {
		t.Errorf("empty user: got %+v want %+v", got, want)
	}
}

func TestResolveBindingsAddsTrigger(t *testing.T) {
	defaults := map[string][]string{
		"new_tab":   {"super+t"},
		"close_tab": {"super+w"},
	}
	got := resolveBindings(defaults, []config.Keybind{
		{Trigger: "super+j", Action: "new_tab"},
	})
	if got["super+t"] != "new_tab" {
		t.Errorf("default super+t lost: %+v", got)
	}
	if got["super+j"] != "new_tab" {
		t.Errorf("super+j not added: %+v", got)
	}
	if got["super+w"] != "close_tab" {
		t.Errorf("close_tab default lost: %+v", got)
	}
}

func TestResolveBindingsUnbindRemovesDefault(t *testing.T) {
	defaults := map[string][]string{
		"new_tab":   {"super+t"},
		"close_tab": {"super+w"},
	}
	got := resolveBindings(defaults, []config.Keybind{
		{Trigger: "super+t", Action: "unbind"},
	})
	if _, ok := got["super+t"]; ok {
		t.Errorf("super+t should be unbound: %+v", got)
	}
	if got["super+w"] != "close_tab" {
		t.Errorf("super+w default lost: %+v", got)
	}
	// new_tab is now unreachable. That's intentional — verify by
	// checking no trigger points at it.
	for _, action := range got {
		if action == "new_tab" {
			t.Errorf("new_tab still reachable after unbind-only-trigger: %+v", got)
		}
	}
}

func TestResolveBindingsReassignTrigger(t *testing.T) {
	defaults := map[string][]string{
		"new_tab":   {"super+t"},
		"close_tab": {"super+w"},
	}
	got := resolveBindings(defaults, []config.Keybind{
		{Trigger: "super+t", Action: "close_tab"},
	})
	if got["super+t"] != "close_tab" {
		t.Errorf("super+t should now map to close_tab: %+v", got)
	}
	if got["super+w"] != "close_tab" {
		t.Errorf("super+w should keep mapping to close_tab: %+v", got)
	}
	// close_tab now has two triggers; new_tab has zero.
	closeCount := 0
	for _, action := range got {
		if action == "close_tab" {
			closeCount++
		}
	}
	if closeCount != 2 {
		t.Errorf("expected close_tab to have 2 triggers, got %d (%+v)", closeCount, got)
	}
}

func TestResolveBindingsIdempotentUnbind(t *testing.T) {
	defaults := map[string][]string{"new_tab": {"super+t"}}
	got := resolveBindings(defaults, []config.Keybind{
		{Trigger: "super+t", Action: "unbind"},
		{Trigger: "super+t", Action: "unbind"},
	})
	if _, ok := got["super+t"]; ok {
		t.Errorf("idempotent unbind failed: %+v", got)
	}
}

func TestResolveBindingsUnbindUnknownTriggerSilent(t *testing.T) {
	defaults := map[string][]string{"new_tab": {"super+t"}}
	got := resolveBindings(defaults, []config.Keybind{
		{Trigger: "super+x", Action: "unbind"},
	})
	if got["super+t"] != "new_tab" {
		t.Errorf("unbinding unknown trigger affected defaults: %+v", got)
	}
}

func TestResolveBindingsLastWinsPerTrigger(t *testing.T) {
	defaults := map[string][]string{}
	got := resolveBindings(defaults, []config.Keybind{
		{Trigger: "super+t", Action: "new_tab"},
		{Trigger: "super+t", Action: "close_tab"},
	})
	if got["super+t"] != "close_tab" {
		t.Errorf("last-wins per trigger: got %+v", got)
	}
}

// TestResolveBindingsUnknownActionSurvivesPipeline verifies that an
// unknown action name is preserved by resolveBindings — the install
// loop is responsible for skipping it (with a slog.Warn). resolveBindings
// is purely structural; semantic validation happens later.
func TestResolveBindingsUnknownActionSurvivesPipeline(t *testing.T) {
	defaults := map[string][]string{}
	got := resolveBindings(defaults, []config.Keybind{
		{Trigger: "super+a", Action: "nonsense_action"},
		{Trigger: "super+b", Action: "new_tab"},
	})
	if got["super+a"] != "nonsense_action" {
		t.Errorf("unknown action dropped at resolve: %+v", got)
	}
	if got["super+b"] != "new_tab" {
		t.Errorf("later valid line not preserved: %+v", got)
	}
}

func mapEq(a, b map[string]string) bool {
	if len(a) != len(b) {
		return false
	}
	keys := make([]string, 0, len(a))
	for k := range a {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	for _, k := range keys {
		if a[k] != b[k] {
			return false
		}
	}
	return true
}
