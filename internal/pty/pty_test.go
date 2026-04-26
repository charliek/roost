package pty

import (
	"errors"
	"io"
	"os"
	"strings"
	"testing"
	"time"

	"github.com/charliek/roost/internal/ghostty"
)

// TestPTYToRenderState exercises the full backend pipeline used by the
// GUI spike, minus GTK: spawn a shell, pump its output bytes into a
// libghostty-vt terminal, and verify the rendered grid contains visible
// text. Proves that PTY + cgo + render-state walking work end-to-end.
func TestPTYToRenderState(t *testing.T) {
	t.Setenv("PS1", "$ ")        // a known prompt
	t.Setenv("SHELL", "/bin/sh") // bypass the user's $SHELL for predictability

	p, err := SpawnShell("", 60, 6)
	if err != nil {
		t.Fatalf("SpawnShell: %v", err)
	}
	t.Cleanup(func() { _ = p.Close() })

	term, err := ghostty.NewTerminal(ghostty.Options{Cols: 60, Rows: 6, MaxScrollback: 200})
	if err != nil {
		t.Fatalf("NewTerminal: %v", err)
	}
	t.Cleanup(term.Close)

	rs, err := ghostty.NewRenderState()
	if err != nil {
		t.Fatalf("NewRenderState: %v", err)
	}
	t.Cleanup(rs.Close)

	// Send a known command then exit. Use a unique sentinel string we
	// can grep for in the rendered grid.
	const sentinel = "ROOST-PTY-OK"
	_, _ = p.Write([]byte("printf '" + sentinel + "\\n'; exit\n"))

	deadline := time.Now().Add(5 * time.Second)
	buf := make([]byte, 4096)
	if f, ok := readDeadliner(p); ok {
		_ = f.SetReadDeadline(deadline)
	}
	for time.Now().Before(deadline) {
		n, err := p.Read(buf)
		if n > 0 {
			term.VTWrite(buf[:n])
		}
		if err != nil {
			if errors.Is(err, io.EOF) || errors.Is(err, os.ErrDeadlineExceeded) {
				break
			}
			break
		}
	}

	if err := rs.Update(term); err != nil {
		t.Fatalf("Update: %v", err)
	}

	// Walk the rendered grid into a row-by-row string, then search.
	var rows [10]strings.Builder
	rs.Walk(func(row, _ int, c ghostty.Cell) {
		if row >= len(rows) || c.Codepoint == 0 {
			return
		}
		rows[row].WriteRune(c.Codepoint)
	})

	var dump strings.Builder
	for i := range rows {
		dump.WriteString(rows[i].String())
		dump.WriteByte('\n')
	}
	t.Logf("rendered grid:\n%s", dump.String())

	if !strings.Contains(dump.String(), sentinel) {
		t.Fatalf("rendered grid did not contain sentinel %q; got:\n%s", sentinel, dump.String())
	}
}

// readDeadliner is a soft type assertion for *os.File embedded in PTY.
// We use it only in tests so we don't bake deadlines into the supervisor.
type fdDeadliner interface {
	SetReadDeadline(time.Time) error
}

func readDeadliner(p *PTY) (fdDeadliner, bool) {
	if p == nil || p.master == nil {
		return nil, false
	}
	return p.master, true
}
