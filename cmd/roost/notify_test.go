package main

import "testing"

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
