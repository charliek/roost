// Package osc is a minimal streaming OSC scanner used as a fallback
// notification path. It runs in parallel to libghostty-vt's parser
// (which still owns the actual VT state) — its only job is to extract
// notification commands so Roost can surface them in the UI.
//
// The scanner handles OSC sequences that may be split across multiple
// PTY reads. It recognizes:
//
//	ESC ] 9 ; <message> BEL              — iTerm2 / general notification
//	ESC ] 9 ; <message> ESC \            — same with ST terminator
//	ESC ] 777 ; notify ; <title> ; <body> BEL  — Konsole / KDE
//
// Other OSC commands are recognized at the prefix level and ignored.
// Bodies longer than maxBody bytes are truncated rather than buffered
// indefinitely (a misbehaving program shouldn't be able to OOM us).
package osc

import "strings"

const maxBody = 8192

// Notification is one extracted OSC notification.
type Notification struct {
	Title string
	Body  string
}

// Scanner is a stateful byte stream parser. Not safe for concurrent use;
// each PTY/Session has its own.
type Scanner struct {
	state state
	num   strings.Builder
	body  strings.Builder
	out   func(Notification)
}

type state int

const (
	stateOutside state = iota
	stateEsc           // saw ESC, waiting for ]
	statePrefix        // collecting <number> before ;
	stateBody          // collecting body before BEL or ESC \
	stateBodyEsc       // saw ESC in body, expecting \
)

// NewScanner returns a scanner that calls fn for every completed
// notification. fn runs synchronously inside Feed and may be invoked
// from any goroutine — make it safe accordingly.
func NewScanner(fn func(Notification)) *Scanner {
	return &Scanner{out: fn}
}

// Feed processes len(p) bytes. The scanner is purely additive — bytes
// are also expected to flow through to the terminal's vt_write
// unchanged.
func (s *Scanner) Feed(p []byte) {
	for _, b := range p {
		s.step(b)
	}
}

func (s *Scanner) step(b byte) {
	switch s.state {
	case stateOutside:
		if b == 0x1B {
			s.state = stateEsc
		}
	case stateEsc:
		switch b {
		case ']':
			s.state = statePrefix
			s.num.Reset()
			s.body.Reset()
		case 0x1B:
			// ESC ESC: stay in stateEsc.
		default:
			s.state = stateOutside
		}
	case statePrefix:
		switch {
		case b == ';':
			s.state = stateBody
		case b == 0x07: // BEL terminator with no body
			s.dispatch()
			s.state = stateOutside
		case b == 0x1B: // ESC \ terminator
			s.state = stateBodyEsc
		case b >= '0' && b <= '9':
			if s.num.Len() < 8 {
				s.num.WriteByte(b)
			}
		case b >= 'a' && b <= 'z', b >= 'A' && b <= 'Z':
			// Some OSC commands use letters (e.g. OSC L). We don't
			// care about those; just keep scanning until terminator.
			if s.num.Len() < 8 {
				s.num.WriteByte(b)
			}
		default:
			// Unexpected character — bail to outside.
			s.state = stateOutside
		}
	case stateBody:
		switch b {
		case 0x07:
			s.dispatch()
			s.state = stateOutside
		case 0x1B:
			s.state = stateBodyEsc
		default:
			if s.body.Len() < maxBody {
				s.body.WriteByte(b)
			}
		}
	case stateBodyEsc:
		if b == '\\' {
			s.dispatch()
			s.state = stateOutside
			return
		}
		// Any byte other than \ aborts the sequence (malformed). Re-feed
		// the byte through the outer state machine so that an ESC
		// starting a new OSC sequence isn't lost — back-to-back OSC
		// notifications terminated with ESC + non-\ would otherwise
		// silently swallow the second sequence's start.
		s.state = stateOutside
		s.step(b)
	}
}

// dispatch is called when a complete OSC sequence has been buffered.
func (s *Scanner) dispatch() {
	if s.out == nil {
		return
	}
	num := s.num.String()
	body := s.body.String()
	switch num {
	case "9":
		// iTerm2 OSC 9: body is the notification message.
		s.out(Notification{Title: body})
	case "777":
		// Konsole OSC 777: body is "notify;<summary>;<body>".
		// Some senders also emit "notify;<summary>" (no body).
		parts := strings.SplitN(body, ";", 3)
		if len(parts) >= 2 && parts[0] == "notify" {
			n := Notification{Title: parts[1]}
			if len(parts) == 3 {
				n.Body = parts[2]
			}
			s.out(n)
		}
	}
}
