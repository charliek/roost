package main

import "testing"

func TestPathDisplay(t *testing.T) {
	cases := []struct {
		name string
		p    string
		home string
		max  int
		want string
	}{
		{"home itself", "/home/charliek", "/home/charliek", 48, "~"},
		{"home child", "/home/charliek/projects/roost", "/home/charliek", 48, "~/projects/roost"},
		{"home prefix not boundary", "/home/charlieknudsen/x", "/home/charliek", 48, "/home/charlieknudsen/x"},
		{"empty home no-op", "/var/log", "", 48, "/var/log"},
		{"unrelated path", "/var/log", "/home/charliek", 48, "/var/log"},
		{"truncate keeps right", "/a/b/c/d/e/f", "", 7, "…/d/e/f"},
		{"no truncate when fits", "/a/b/c", "", 10, "/a/b/c"},
		{"truncate respects runes", "/aaaa/🐓🐓🐓", "", 6, "…a/🐓🐓🐓"},
		{"home then truncate", "/home/charliek/very/deep/tree/leaf", "/home/charliek", 12, "…p/tree/leaf"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			got := pathDisplay(tc.p, tc.home, tc.max)
			if got != tc.want {
				t.Errorf("pathDisplay(%q, %q, %d) = %q; want %q", tc.p, tc.home, tc.max, got, tc.want)
			}
		})
	}
}
