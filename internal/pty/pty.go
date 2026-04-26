// Package pty owns the pseudo-terminal lifecycle for one tab. Each PTY
// runs a single child shell; reads happen on a goroutine, writes happen
// from the GTK main thread (or anywhere — writes are short and the
// kernel buffers them).
package pty

import (
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"

	"github.com/creack/pty"
)

// PTY is a running shell with its master file descriptor.
type PTY struct {
	cmd    *exec.Cmd
	master *os.File
}

// SpawnShell starts $SHELL inside a fresh PTY at the given grid size.
// Returns a PTY whose master fd is ready for read/write. Caller must
// eventually call Close.
func SpawnShell(cwd string, cols, rows uint16) (*PTY, error) {
	shell := os.Getenv("SHELL")
	if shell == "" {
		shell = "/bin/sh"
	}

	cmd := exec.Command(shell)
	cmd.Env = append(os.Environ(),
		"TERM=xterm-256color",
		"COLORTERM=truecolor",
	)
	if cwd != "" {
		cmd.Dir = cwd
	}

	master, err := pty.StartWithSize(cmd, &pty.Winsize{Rows: rows, Cols: cols})
	if err != nil {
		return nil, fmt.Errorf("pty.StartWithSize: %w", err)
	}
	return &PTY{cmd: cmd, master: master}, nil
}

// Read drains bytes from the PTY. Returns io.EOF when the child closes
// its side. Block until data is available.
func (p *PTY) Read(b []byte) (int, error) {
	n, err := p.master.Read(b)
	// On Linux, EIO from a closed slave is the equivalent of EOF.
	if err != nil && errors.Is(err, syscallEIO()) {
		return n, io.EOF
	}
	return n, err
}

// Write sends bytes to the PTY (i.e. as if the user typed them).
func (p *PTY) Write(b []byte) (int, error) {
	return p.master.Write(b)
}

// Resize updates the kernel's record of the PTY's grid size and pixel
// size. Triggers SIGWINCH in the foreground process group.
func (p *PTY) Resize(cols, rows, cellW, cellH uint16) error {
	return pty.Setsize(p.master, &pty.Winsize{
		Rows: rows, Cols: cols,
		X: cols * cellW, Y: rows * cellH,
	})
}

// Close kills the child if alive and releases the master fd. Safe to call
// multiple times.
func (p *PTY) Close() error {
	if p.master != nil {
		_ = p.master.Close()
		p.master = nil
	}
	if p.cmd != nil && p.cmd.Process != nil {
		_ = p.cmd.Process.Kill()
		_, _ = p.cmd.Process.Wait()
		p.cmd = nil
	}
	return nil
}
