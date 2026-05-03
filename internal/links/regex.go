// Package links detects URL spans inside terminal row text. Used by the
// renderer / input layer to underline and follow URLs the user hovers
// with a modifier key held (Ctrl on Linux, Cmd on macOS).
//
// Detection is regex-based and intentionally narrow: only well-known URL
// schemes match. Path detection (./foo, ~/bar, src/main.go:42) and
// scp-style git remotes (git@github.com:x/y) are out of scope.
package links

import (
	"regexp"
	"strings"
)

// urlPattern matches schemed URLs. Greedy match of any non-whitespace,
// non-quote character after the scheme; trailing punctuation and
// unmatched closing brackets are stripped post-match by trimURL.
var urlPattern = regexp.MustCompile(`(?i)(?:(?:https?|ftp|file|ssh|git\+ssh)://|(?:mailto|tel|news):)[^\s<>"'` + "`" + `\\]+`)

// Span describes one URL match within a row. Col0 is the first column
// covered (inclusive); Col1 is the last column covered (inclusive). URL
// is the cleaned, trim-trailing-punctuation form of the match.
type Span struct {
	Col0, Col1 int
	URL        string
}

// FindAt scans `row` for a URL whose column range contains `col` and
// returns the matching span. Returns (Span{}, false) if no URL straddles
// that column.
//
// `row` is the rune slice of the visible row's text (one entry per cell;
// spaces for empty cells). Column indices in the returned Span match
// rune indices in `row`.
func FindAt(row []rune, col int) (Span, bool) {
	if len(row) == 0 || col < 0 || col >= len(row) {
		return Span{}, false
	}
	// Convert the rune slice to a string for regex; build a parallel
	// byte→rune index map so we can translate match offsets back. Each
	// entry stores the BYTE offset where rune i starts (rune i may be
	// multi-byte under UTF-8); the final entry holds the total byte
	// length so the lookup can resolve an end-of-string offset.
	var b strings.Builder
	b.Grow(len(row))
	byteToRune := make([]int, 0, len(row)+1)
	byteOff := 0
	for _, r := range row {
		byteToRune = append(byteToRune, byteOff)
		n, _ := b.WriteRune(r)
		byteOff += n
	}
	byteToRune = append(byteToRune, byteOff)
	s := b.String()

	matches := urlPattern.FindAllStringIndex(s, -1)
	for _, m := range matches {
		raw := s[m[0]:m[1]]
		trimmed := trimURL(raw)
		if trimmed == "" {
			continue
		}
		// Column span (in runes) of the trimmed match.
		startCol := byteToRuneIndex(byteToRune, m[0])
		// m[0] + len(trimmed) is a byte offset in s; map it back.
		endCol := byteToRuneIndex(byteToRune, m[0]+len(trimmed)) - 1
		if endCol < startCol {
			continue
		}
		if col >= startCol && col <= endCol {
			return Span{Col0: startCol, Col1: endCol, URL: trimmed}, true
		}
	}
	return Span{}, false
}

// byteToRuneIndex maps a byte offset within the row's string form back
// to a rune (= column) index, using the precomputed table.
func byteToRuneIndex(table []int, byteOff int) int {
	// Linear scan is fine: rows are short (terminal width).
	for i, off := range table {
		if off >= byteOff {
			return i
		}
	}
	return len(table) - 1
}

// trimURL strips trailing punctuation and unmatched closing brackets
// from a URL match. Mirrors what ghostty's url.zig and cmux's path
// trimmer do — sentence punctuation almost never belongs to the URL,
// and a trailing `)` is part of the URL only if there was a matching
// `(` inside it (Wikipedia-style `_(disambiguation)` links).
func trimURL(u string) string {
	// First, strip plain trailing punctuation that is never part of a URL.
	for len(u) > 0 {
		last := u[len(u)-1]
		switch last {
		case '.', ',', ';', ':', '!', '?':
			u = u[:len(u)-1]
		default:
			goto bracketPass
		}
	}
bracketPass:
	// Then balance unmatched closing brackets.
	for len(u) > 0 {
		last := u[len(u)-1]
		var open byte
		switch last {
		case ')':
			open = '('
		case ']':
			open = '['
		case '}':
			open = '{'
		default:
			return u
		}
		// Count opens and closes inside the candidate.
		opens := strings.Count(u, string(open))
		closes := strings.Count(u, string(last))
		if closes > opens {
			u = u[:len(u)-1]
			continue
		}
		return u
	}
	return u
}
