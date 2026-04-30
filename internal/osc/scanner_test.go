package osc

import "testing"

func collect(t *testing.T) (*Scanner, *[]Notification) {
	t.Helper()
	var got []Notification
	return NewScanner(Handler{
		OnNotification: func(n Notification) { got = append(got, n) },
	}), &got
}

// collectQuery returns a scanner configured with the given fg/bg/cursor
// colours and a slice that captures every synthesised query response.
func collectQuery(t *testing.T, fg, bg, cursor RGB) (*Scanner, *[][]byte) {
	t.Helper()
	var got [][]byte
	s := NewScanner(Handler{
		OnQueryResponse: func(b []byte) {
			got = append(got, append([]byte(nil), b...))
		},
		QueryColors: func() (RGB, RGB, RGB) { return fg, bg, cursor },
	})
	return s, &got
}

func TestOSC9SingleChunk(t *testing.T) {
	s, got := collect(t)
	s.Feed([]byte("\x1b]9;Build done\x07"))
	if len(*got) != 1 || (*got)[0].Title != "Build done" {
		t.Fatalf("got %+v", *got)
	}
}

func TestOSC9SplitAcrossReads(t *testing.T) {
	s, got := collect(t)
	s.Feed([]byte("hello \x1b]9;part one"))
	if len(*got) != 0 {
		t.Fatalf("premature dispatch: %+v", *got)
	}
	s.Feed([]byte(" part two\x07 trailing"))
	if len(*got) != 1 || (*got)[0].Title != "part one part two" {
		t.Fatalf("got %+v", *got)
	}
}

func TestOSC9STTerminator(t *testing.T) {
	s, got := collect(t)
	s.Feed([]byte("\x1b]9;hello\x1b\\"))
	if len(*got) != 1 || (*got)[0].Title != "hello" {
		t.Fatalf("got %+v", *got)
	}
}

func TestOSC777WithBody(t *testing.T) {
	s, got := collect(t)
	s.Feed([]byte("\x1b]777;notify;Title;Body text\x07"))
	if len(*got) != 1 || (*got)[0].Title != "Title" || (*got)[0].Body != "Body text" {
		t.Fatalf("got %+v", *got)
	}
}

func TestOSC777NoBody(t *testing.T) {
	s, got := collect(t)
	s.Feed([]byte("\x1b]777;notify;Just a title\x07"))
	if len(*got) != 1 || (*got)[0].Title != "Just a title" {
		t.Fatalf("got %+v", *got)
	}
}

func TestOSC9ConEmuProgressIgnored(t *testing.T) {
	s, got := collect(t)
	// ConEmu OSC 9;4 is a progress report; Claude Code emits these
	// constantly. Body the scanner sees is "4;0;" (state=remove).
	s.Feed([]byte("\x1b]9;4;0;\x07"))
	s.Feed([]byte("\x1b]9;4;1;30\x07"))
	if len(*got) != 0 {
		t.Fatalf("ConEmu progress fired notification: %+v", *got)
	}
}

func TestOSC9ConEmuOtherSubcommandsIgnored(t *testing.T) {
	s, got := collect(t)
	// 9;1 sleep, 9;11 comment, 9;2 message box. All ConEmu.
	s.Feed([]byte("\x1b]9;1;100\x07"))
	s.Feed([]byte("\x1b]9;11;just a comment\x07"))
	s.Feed([]byte("\x1b]9;2;some message\x07"))
	if len(*got) != 0 {
		t.Fatalf("ConEmu subcommands fired notification: %+v", *got)
	}
}

func TestOSC9NumericOutOfConEmuRangeFires(t *testing.T) {
	// ConEmu defines codes 1..12 only. A body like "42;summary" is not
	// ConEmu — treat it as an iTerm2 notification with a numeric-prefix
	// title. The same goes for 0; ConEmu has no zeroth code.
	s, got := collect(t)
	s.Feed([]byte("\x1b]9;42;summary\x07"))
	s.Feed([]byte("\x1b]9;0\x07"))
	if len(*got) != 2 {
		t.Fatalf("expected two notifications, got %d: %+v", len(*got), *got)
	}
	if (*got)[0].Title != "42;summary" || (*got)[1].Title != "0" {
		t.Fatalf("titles: %+v", *got)
	}
}

func TestOSC9DigitTitlePassthrough(t *testing.T) {
	// Regression: a bare iTerm2 notification whose title HAPPENS to
	// start with a digit followed by a non-`;` byte must still fire.
	s, got := collect(t)
	s.Feed([]byte("\x1b]9;1 file changed\x07"))
	if len(*got) != 1 || (*got)[0].Title != "1 file changed" {
		t.Fatalf("digit-prefixed iTerm2 notification dropped: %+v", *got)
	}
}

func TestUnrelatedOSCIgnored(t *testing.T) {
	s, got := collect(t)
	// OSC 0 (icon+title) and OSC 1 (icon) — should NOT fire.
	s.Feed([]byte("\x1b]0;some title\x07"))
	s.Feed([]byte("\x1b]1;icon\x07"))
	s.Feed([]byte("\x1b]7;file:///tmp\x07"))
	if len(*got) != 0 {
		t.Fatalf("non-notification OSC fired: %+v", *got)
	}
}

func TestPlainBytesUnaffected(t *testing.T) {
	s, got := collect(t)
	s.Feed([]byte("regular shell output\nls -la\nmore output"))
	if len(*got) != 0 {
		t.Fatalf("plain bytes fired notification: %+v", *got)
	}
}

func TestMalformedRecoversToOutside(t *testing.T) {
	s, got := collect(t)
	// ESC ] foo BEL — non-numeric prefix is accepted as letters but
	// dispatch matches no case. Then a real OSC 9 follows.
	s.Feed([]byte("\x1b]foo\x07\x1b]9;real\x07"))
	if len(*got) != 1 || (*got)[0].Title != "real" {
		t.Fatalf("got %+v", *got)
	}
}

// Back-to-back ESC-terminated OSC sequences where the first's
// trailing ESC is followed by an ESC starting the second instead of a
// `\`. The first should be discarded as malformed and the second
// should still fire.
func TestOSCBackToBackAfterMalformedSTRecovers(t *testing.T) {
	s, got := collect(t)
	s.Feed([]byte("\x1b]9;first\x1b\x1b]9;second\x07"))
	if len(*got) != 1 || (*got)[0].Title != "second" {
		t.Fatalf("got %+v", *got)
	}
}

func TestBodyTruncatedAtMax(t *testing.T) {
	s, got := collect(t)
	s.Feed([]byte("\x1b]9;"))
	huge := make([]byte, maxBody+1000)
	for i := range huge {
		huge[i] = 'a'
	}
	s.Feed(huge)
	s.Feed([]byte{0x07})
	if len(*got) != 1 {
		t.Fatalf("expected one notification, got %d", len(*got))
	}
	if len((*got)[0].Title) != maxBody {
		t.Fatalf("title len: got %d want %d", len((*got)[0].Title), maxBody)
	}
}

// --- OSC 10/11/12 query response synthesis ---------------------------

var (
	tFG     = RGB{R: 0xFF, G: 0xFF, B: 0xFF}
	tBG     = RGB{R: 0x1E, G: 0x1E, B: 0x1E}
	tCursor = RGB{R: 0x98, G: 0x98, B: 0x9D}
)

func TestOSC10QueryBELForeground(t *testing.T) {
	s, got := collectQuery(t, tFG, tBG, tCursor)
	s.Feed([]byte("\x1b]10;?\x07"))
	want := []byte("\x1b]10;rgb:ffff/ffff/ffff\x07")
	if len(*got) != 1 || string((*got)[0]) != string(want) {
		t.Fatalf("got %q want %q", *got, want)
	}
}

func TestOSC11QueryBELBackground(t *testing.T) {
	s, got := collectQuery(t, tFG, tBG, tCursor)
	s.Feed([]byte("\x1b]11;?\x07"))
	want := []byte("\x1b]11;rgb:1e1e/1e1e/1e1e\x07")
	if len(*got) != 1 || string((*got)[0]) != string(want) {
		t.Fatalf("got %q want %q", *got, want)
	}
}

func TestOSC12QueryBELCursor(t *testing.T) {
	s, got := collectQuery(t, tFG, tBG, tCursor)
	s.Feed([]byte("\x1b]12;?\x07"))
	want := []byte("\x1b]12;rgb:9898/9898/9d9d\x07")
	if len(*got) != 1 || string((*got)[0]) != string(want) {
		t.Fatalf("got %q want %q", *got, want)
	}
}

func TestOSC11QuerySTTerminator(t *testing.T) {
	s, got := collectQuery(t, tFG, tBG, tCursor)
	s.Feed([]byte("\x1b]11;?\x1b\\"))
	// Response always uses BEL regardless of which terminator the
	// query used; both forms are universally accepted.
	want := []byte("\x1b]11;rgb:1e1e/1e1e/1e1e\x07")
	if len(*got) != 1 || string((*got)[0]) != string(want) {
		t.Fatalf("got %q want %q", *got, want)
	}
}

func TestOSC11QuerySplitAcrossFeeds(t *testing.T) {
	s, got := collectQuery(t, tFG, tBG, tCursor)
	// Split right between the `?` and the BEL.
	s.Feed([]byte("\x1b]11;?"))
	if len(*got) != 0 {
		t.Fatalf("premature dispatch: %q", *got)
	}
	s.Feed([]byte("\x07"))
	if len(*got) != 1 {
		t.Fatalf("expected one response after second Feed, got %q", *got)
	}
}

func TestOSC11SetColorIsNotAQuery(t *testing.T) {
	// libghostty handles set/reset colour operations correctly. Our
	// scanner must NOT respond to them — only to body == "?".
	s, got := collectQuery(t, tFG, tBG, tCursor)
	s.Feed([]byte("\x1b]11;rgb:00/00/00\x07"))
	s.Feed([]byte("\x1b]10;#ffffff\x07"))
	if len(*got) != 0 {
		t.Fatalf("set-colour OSCs incorrectly produced responses: %q", *got)
	}
}

func TestQueryResponseSkippedWhenHandlerNil(t *testing.T) {
	// Notification-only handler: no OnQueryResponse, no QueryColors.
	// OSC 11 query must be silently ignored, not panic.
	var notes []Notification
	s := NewScanner(Handler{
		OnNotification: func(n Notification) { notes = append(notes, n) },
	})
	s.Feed([]byte("\x1b]11;?\x07"))
	if len(notes) != 0 {
		t.Fatalf("OSC 11 query produced notifications: %+v", notes)
	}
}

func TestQueryAndNotificationCoexist(t *testing.T) {
	// One scanner handling both notification and query callbacks must
	// route each OSC class correctly without crosstalk.
	var notes []Notification
	var resps [][]byte
	s := NewScanner(Handler{
		OnNotification:  func(n Notification) { notes = append(notes, n) },
		OnQueryResponse: func(b []byte) { resps = append(resps, append([]byte(nil), b...)) },
		QueryColors:     func() (RGB, RGB, RGB) { return tFG, tBG, tCursor },
	})
	s.Feed([]byte("\x1b]9;hello\x07\x1b]11;?\x07\x1b]9;world\x07"))
	if len(notes) != 2 || notes[0].Title != "hello" || notes[1].Title != "world" {
		t.Fatalf("notifications: %+v", notes)
	}
	if len(resps) != 1 || string(resps[0]) != "\x1b]11;rgb:1e1e/1e1e/1e1e\x07" {
		t.Fatalf("responses: %q", resps)
	}
}
