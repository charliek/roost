package main

import "testing"

func TestEscapeApplescript(t *testing.T) {
	cases := []struct {
		in, want string
	}{
		{"", ""},
		{"plain text", "plain text"},
		{`one "quote"`, `one \"quote\"`},
		{`back\slash`, `back\\slash`},
		{"line1\nline2", `line1\nline2`},
		{"line1\r\nline2", `line1\r\nline2`},
		{"mix \"q\" \\ \nend", `mix \"q\" \\ \nend`},
	}
	for _, c := range cases {
		if got := escapeApplescript(c.in); got != c.want {
			t.Errorf("escapeApplescript(%q) = %q, want %q", c.in, got, c.want)
		}
	}
}
