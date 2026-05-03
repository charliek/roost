package links

import "testing"

func TestFindAt(t *testing.T) {
	type tc struct {
		name    string
		row     string
		col     int
		wantURL string
		wantOK  bool
		col0    int
		col1    int
	}
	cases := []tc{
		{
			name:    "bare https mid-string",
			row:     "see https://example.com for details",
			col:     12,
			wantURL: "https://example.com",
			wantOK:  true,
			col0:    4,
			col1:    22,
		},
		{
			name:    "github PR url",
			row:     "Created PR https://github.com/charliek/roost/pull/42",
			col:     30,
			wantURL: "https://github.com/charliek/roost/pull/42",
			wantOK:  true,
			col0:    11,
			col1:    51,
		},
		{
			name:    "trailing period stripped",
			row:     "Visit https://example.com.",
			col:     10,
			wantURL: "https://example.com",
			wantOK:  true,
		},
		{
			name:    "trailing comma stripped",
			row:     "links: https://a.test, https://b.test",
			col:     10,
			wantURL: "https://a.test",
			wantOK:  true,
		},
		{
			name:    "wikipedia parenthesized url kept whole",
			row:     "see https://en.wikipedia.org/wiki/Rust_(programming_language) here",
			col:     20,
			wantURL: "https://en.wikipedia.org/wiki/Rust_(programming_language)",
			wantOK:  true,
		},
		{
			name:    "url inside parens drops trailing close-paren",
			row:     "see (https://example.com) here",
			col:     10,
			wantURL: "https://example.com",
			wantOK:  true,
		},
		{
			name:    "mailto scheme",
			row:     "mail mailto:a@b.com please",
			col:     8,
			wantURL: "mailto:a@b.com",
			wantOK:  true,
		},
		{
			name:    "file uri",
			row:     "open file:///tmp/foo.txt now",
			col:     10,
			wantURL: "file:///tmp/foo.txt",
			wantOK:  true,
		},
		{
			name:   "no scheme = no match",
			row:    "this is not.a.url at all",
			col:    10,
			wantOK: false,
		},
		{
			name:   "scp git remote not matched",
			row:    "remote git@github.com:x/y.git origin",
			col:    14,
			wantOK: false,
		},
		{
			name:   "col outside any match",
			row:    "see https://a.test here",
			col:    20,
			wantOK: false,
		},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			row := []rune(c.row)
			got, ok := FindAt(row, c.col)
			if ok != c.wantOK {
				t.Fatalf("ok = %v, want %v (got %+v)", ok, c.wantOK, got)
			}
			if !ok {
				return
			}
			if got.URL != c.wantURL {
				t.Errorf("URL = %q, want %q", got.URL, c.wantURL)
			}
			if c.col0 != 0 || c.col1 != 0 {
				if got.Col0 != c.col0 {
					t.Errorf("Col0 = %d, want %d", got.Col0, c.col0)
				}
				if got.Col1 != c.col1 {
					t.Errorf("Col1 = %d, want %d", got.Col1, c.col1)
				}
			}
		})
	}
}

func TestTrimURL(t *testing.T) {
	cases := map[string]string{
		"https://x.test":               "https://x.test",
		"https://x.test.":              "https://x.test",
		"https://x.test,":              "https://x.test",
		"https://x.test);":             "https://x.test",
		"https://w.org/Rust_(lang)":    "https://w.org/Rust_(lang)",
		"https://w.org/Rust_(lang).":   "https://w.org/Rust_(lang)",
		"https://x.test])":             "https://x.test",
		"https://x.test/(a)b":          "https://x.test/(a)b",
	}
	for in, want := range cases {
		t.Run(in, func(t *testing.T) {
			if got := trimURL(in); got != want {
				t.Errorf("trimURL(%q) = %q, want %q", in, got, want)
			}
		})
	}
}
