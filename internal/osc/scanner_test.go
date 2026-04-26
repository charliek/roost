package osc

import "testing"

func collect(t *testing.T) (*Scanner, *[]Notification) {
	t.Helper()
	var got []Notification
	return NewScanner(func(n Notification) { got = append(got, n) }), &got
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
