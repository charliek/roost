package main

import "testing"

// TestNotifierExecCmd locks in the exact command-line shape used for
// terminal-notifier click-through. roost-cli changed from
// `tab focus --tab N` to `tab focus N` (positional) — this test makes
// the wire format observable so it can't drift silently.
func TestNotifierExecCmd(t *testing.T) {
	cases := []struct {
		cliPath string
		tabID   int64
		want    string
	}{
		{"/usr/local/bin/roost-cli", 7, "/usr/local/bin/roost-cli tab focus 7"},
		{"/with space/roost-cli", 42, "'/with space/roost-cli' tab focus 42"},
	}
	for _, c := range cases {
		if got := notifierExecCmd(c.cliPath, c.tabID); got != c.want {
			t.Errorf("notifierExecCmd(%q, %d) = %q, want %q", c.cliPath, c.tabID, got, c.want)
		}
	}
}

func TestQuoteForExecute(t *testing.T) {
	cases := []struct {
		in, want string
	}{
		{"plain", "plain"},
		{"/Users/charliek/.local/bin/roost-cli", "/Users/charliek/.local/bin/roost-cli"},
		{"/with space/cli", "'/with space/cli'"},
		{"/with$dollar/cli", "'/with$dollar/cli'"},
		{`/with\backslash/cli`, `'/with\backslash/cli'`},
		{`/with"quote/cli`, `'/with"quote/cli'`},
		{"/with'apostrophe/cli", `'/with'\''apostrophe/cli'`},
	}
	for _, c := range cases {
		if got := quoteForExecute(c.in); got != c.want {
			t.Errorf("quoteForExecute(%q) = %q, want %q", c.in, got, c.want)
		}
	}
}
