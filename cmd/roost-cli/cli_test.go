package main

import (
	"strings"
	"testing"
	"time"

	"github.com/spf13/cobra"
)

// resetFlagsForTest resets package-level flag globals so tests don't
// leak state into each other. Cobra is not designed for repeated
// in-process Execute() calls, so this is best-effort: we re-bind the
// flags rather than try to fully reconstruct rootCmd.
func resetFlagsForTest(t *testing.T) {
	t.Helper()
	prev := struct {
		socket   string
		json     bool
		timeout  time.Duration
		verbose  int
		notify   int64
		setTitle int64
		setState int64
		ctx      clientCtxT
	}{flagSocket, flagJSON, flagTimeout, flagVerbose, notifyTabFlag, tabSetTitleFlag, tabSetStateFlag, clientCtx}
	t.Cleanup(func() {
		flagSocket = prev.socket
		flagJSON = prev.json
		flagTimeout = prev.timeout
		flagVerbose = prev.verbose
		notifyTabFlag = prev.notify
		tabSetTitleFlag = prev.setTitle
		tabSetStateFlag = prev.setState
		clientCtx = prev.ctx
	})

	flagSocket = ""
	flagJSON = false
	flagTimeout = 3 * time.Second
	flagVerbose = 0
	notifyTabFlag = 0
	tabSetTitleFlag = 0
	tabSetStateFlag = 0
	clientCtx = clientCtxT{}
}

// runRoot invokes rootCmd with the given args and returns the error.
// We use cobra's silent error path so test output stays clean.
func runRoot(t *testing.T, args ...string) error {
	t.Helper()
	rootCmd.SetArgs(args)
	rootCmd.SetOut(discardWriter{})
	rootCmd.SetErr(discardWriter{})
	t.Cleanup(func() {
		rootCmd.SetArgs(nil)
		rootCmd.SetOut(nil)
		rootCmd.SetErr(nil)
	})
	return rootCmd.Execute()
}

type discardWriter struct{}

func (discardWriter) Write(p []byte) (int, error) { return len(p), nil }

func TestSocketPrecedenceFlagWinsOverEnv(t *testing.T) {
	resetFlagsForTest(t)
	t.Setenv("ROOST_SOCKET", "/env/socket")

	// Use a non-existent socket so the connect fails — we just want
	// to observe which path was used in the error message.
	_ = captureStderr(t, func() {
		_ = captureStdout(t, func() {
			_ = runRoot(t, "--socket", "/flag/socket", "tab", "list")
		})
	})

	if clientCtx.SocketPath != "/flag/socket" {
		t.Errorf("expected --socket to win; got %q", clientCtx.SocketPath)
	}
}

func TestSocketPrecedenceEnvWinsOverDefault(t *testing.T) {
	resetFlagsForTest(t)
	t.Setenv("ROOST_SOCKET", "/env/socket")

	_ = captureStderr(t, func() {
		_ = captureStdout(t, func() {
			_ = runRoot(t, "tab", "list")
		})
	})

	if clientCtx.SocketPath != "/env/socket" {
		t.Errorf("expected env to win without --socket; got %q", clientCtx.SocketPath)
	}
}

func TestVersionWorksWithoutIPC(t *testing.T) {
	resetFlagsForTest(t)
	// No socket; PersistentPreRunE skip-list must keep this command
	// off the IPC resolution path.
	t.Setenv("ROOST_SOCKET", "")

	out := captureStdout(t, func() {
		if err := runRoot(t, "version"); err != nil {
			t.Fatalf("version: %v", err)
		}
	})
	if !strings.Contains(out, "roost-cli ") {
		t.Errorf("version output: %q", out)
	}
}

func TestVersionJSONMode(t *testing.T) {
	resetFlagsForTest(t)
	out := captureStdout(t, func() {
		if err := runRoot(t, "--json", "version"); err != nil {
			t.Fatalf("version --json: %v", err)
		}
	})
	if !strings.Contains(out, `"version"`) {
		t.Errorf("expected JSON, got %q", out)
	}
}

func TestCompletionWorksWithoutIPC(t *testing.T) {
	resetFlagsForTest(t)
	out := captureStdout(t, func() {
		if err := runRoot(t, "completion", "bash"); err != nil {
			t.Fatalf("completion bash: %v", err)
		}
	})
	if !strings.Contains(out, "bash completion") && !strings.Contains(out, "_roost-cli") {
		t.Errorf("expected a bash completion script, got first 200 chars: %q", out[:min(len(out), 200)])
	}
}

func TestTabSetTitleRejectsOldFlagForm(t *testing.T) {
	resetFlagsForTest(t)
	// --title is no longer a flag on tab set-title; we expect a parse
	// error wrapped as a usage error.
	err := runRoot(t, "tab", "set-title", "--title", "x")
	if err == nil {
		t.Fatal("expected error when using removed --title flag")
	}
}

func TestTabSetStateRejectsOldFlagForm(t *testing.T) {
	resetFlagsForTest(t)
	err := runRoot(t, "tab", "set-state", "--state", "running")
	if err == nil {
		t.Fatal("expected error when using removed --state flag")
	}
}

func TestNotifyRequiresTitle(t *testing.T) {
	resetFlagsForTest(t)
	err := runRoot(t, "notify")
	if err == nil {
		t.Fatal("expected error when notify called with no args")
	}
}

func TestTabFocusInvalidIDIsUsageError(t *testing.T) {
	resetFlagsForTest(t)
	err := runRoot(t, "tab", "focus", "not-a-number")
	if err == nil {
		t.Fatal("expected error for non-integer TAB_ID")
	}
	if !isUsageError(err) {
		t.Errorf("expected errUsage classification, got %v", err)
	}
}

// isUsageError mirrors main()'s classification.
func isUsageError(err error) bool {
	for ; err != nil; err = unwrap(err) {
		if err == errUsage {
			return true
		}
	}
	return false
}

func unwrap(err error) error {
	type wrapper interface{ Unwrap() error }
	if w, ok := err.(wrapper); ok {
		return w.Unwrap()
	}
	return nil
}

// silence "imported and not used: cobra" if all branches go unused
var _ = cobra.Command{}
