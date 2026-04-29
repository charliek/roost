// Package osc is a minimal streaming OSC scanner used as a fallback
// path next to libghostty-vt's parser. libghostty owns the actual VT
// state; this scanner observes the same byte stream to handle two
// classes of OSC that libghostty either doesn't surface or doesn't
// answer:
//
//  1. Notifications (OSC 9, OSC 777) — extracted and forwarded to the
//     workspace UI via OnNotification.
//  2. Color queries (OSC 10/11/12 with body "?") — synthesised into the
//     `\e]Ps;rgb:RRRR/GGGG/BBBB\a` response and emitted via
//     OnQueryResponse, since libghostty-vt drops the .query arm of OSC
//     color operations (see ../../ghostty/src/terminal/stream_terminal.zig:616-618).
//     Without this Codex doesn't know our background colour and skips
//     the gray-bar BG SGR for its prompt row.
//
// The scanner handles OSC sequences split across multiple PTY reads.
// Bodies longer than maxBody bytes are truncated rather than buffered
// indefinitely (a misbehaving program shouldn't be able to OOM us).
package osc

import (
	"fmt"
	"strings"
)

const maxBody = 8192

// Notification is one extracted OSC notification.
type Notification struct {
	Title string
	Body  string
}

// RGB is an 8-bit-per-channel colour. Defined here so the package has
// no upstream dependencies; callers convert from their own type.
type RGB struct{ R, G, B uint8 }

// Handler is the set of callbacks Scanner invokes. Any field may be
// nil; the scanner skips the matching OSC class silently in that case.
type Handler struct {
	// OnNotification fires when an OSC 9 / OSC 777 notification is parsed.
	OnNotification func(Notification)

	// OnQueryResponse receives raw bytes that should be written back to
	// the pty (OSC 10/11/12 query responses we synthesise). The caller
	// is responsible for actually writing them; the scanner only emits.
	// The slice is owned by the receiver — safe to retain.
	OnQueryResponse func([]byte)

	// QueryColors returns the (foreground, background, cursor) colours
	// to use for OSC 10/11/12 query responses. Called once per query.
	// Required if OnQueryResponse is set.
	QueryColors func() (fg, bg, cursor RGB)
}

// Scanner is a stateful byte stream parser. Not safe for concurrent
// use; each PTY/Session has its own.
type Scanner struct {
	state state
	num   strings.Builder
	body  strings.Builder
	h     Handler
}

type state int

const (
	stateOutside state = iota
	stateEsc           // saw ESC, waiting for ]
	statePrefix        // collecting <number> before ;
	stateBody          // collecting body before BEL or ESC \
	stateBodyEsc       // saw ESC in body, expecting \
)

// NewScanner returns a scanner that invokes the callbacks in h for
// matching OSC sequences. Callbacks run synchronously inside Feed.
func NewScanner(h Handler) *Scanner {
	return &Scanner{h: h}
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

// isConEmuBody reports whether an OSC 9 body looks like a ConEmu
// extension rather than an iTerm2 notification. ConEmu uses OSC 9;<n>
// for n in 1..12 (sleep, message-box, change-tab-title, progress,
// wait-input, GUI-macro, run-process, env-var, xterm-emulation,
// comment); the body we see (everything after the leading "9;") starts
// with that decimal number and is either bare or followed by ';'.
//
// A bare numeric body outside the 1..12 range (e.g. "9;42;summary") is
// treated as an iTerm2 notification — ConEmu doesn't define those, so
// the conservative interpretation is that a sender used a numeric
// title. A genuine iTerm2 notification whose text starts with a digit
// followed by any non-digit byte (e.g. "1 file changed") still
// passes through.
func isConEmuBody(body string) bool {
	if body == "" || body[0] < '0' || body[0] > '9' {
		return false
	}
	n := 0
	i := 0
	for i < len(body) && body[i] >= '0' && body[i] <= '9' {
		// Cap to keep this from overflowing on absurd inputs; any
		// number of digits here is already out of range anyway.
		if n < 100 {
			n = n*10 + int(body[i]-'0')
		} else {
			n = 100
		}
		i++
	}
	if n < 1 || n > 12 {
		return false
	}
	return i == len(body) || body[i] == ';'
}

// dispatch is called when a complete OSC sequence has been buffered.
func (s *Scanner) dispatch() {
	num := s.num.String()
	body := s.body.String()
	switch num {
	case "9":
		// OSC 9 is overloaded: iTerm2 uses the body as a notification
		// message, but ConEmu uses OSC 9;<n>[;...] for sleep/progress/
		// message-box/etc. (see ../ghostty/src/terminal/osc/parsers/osc9.zig).
		// A digit followed by `;` (or end of body) signals ConEmu — drop
		// it; libghostty-vt parses the actual semantics. Bodies starting
		// with a digit followed by something else (e.g. "1 file changed")
		// are still legitimate iTerm2 notifications.
		if isConEmuBody(body) {
			return
		}
		if s.h.OnNotification != nil {
			s.h.OnNotification(Notification{Title: body})
		}
	case "777":
		// Konsole OSC 777: body is "notify;<summary>;<body>".
		// Some senders also emit "notify;<summary>" (no body).
		if s.h.OnNotification == nil {
			return
		}
		parts := strings.SplitN(body, ";", 3)
		if len(parts) >= 2 && parts[0] == "notify" {
			n := Notification{Title: parts[1]}
			if len(parts) == 3 {
				n.Body = parts[2]
			}
			s.h.OnNotification(n)
		}
	case "10", "11", "12":
		// Dynamic-color queries. Body of exactly "?" means a query for
		// the current foreground (10), background (11), or cursor (12).
		// Anything else is a set/reset operation that libghostty
		// already handles correctly.
		if body != "?" {
			return
		}
		if s.h.OnQueryResponse == nil || s.h.QueryColors == nil {
			return
		}
		fg, bg, cursor := s.h.QueryColors()
		var c RGB
		switch num {
		case "10":
			c = fg
		case "11":
			c = bg
		case "12":
			c = cursor
		}
		// Response uses the 16-bit-per-channel xterm form (RR repeated
		// to fill 4 hex digits). BEL terminator — universally accepted
		// and matches what Codex emits for its own OSCs.
		resp := fmt.Sprintf("\x1b]%s;rgb:%02x%02x/%02x%02x/%02x%02x\x07",
			num, c.R, c.R, c.G, c.G, c.B, c.B)
		s.h.OnQueryResponse([]byte(resp))
	}
}
