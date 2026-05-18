package main

import (
	"bytes"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

// builtBinaryPath is set by TestMain. The build happens once per test
// run; tests skip when -short is passed.
var builtBinaryPath string

func TestMain(m *testing.M) {
	// Build the binary unconditionally — testing.Short() can't be
	// queried here yet (flag.Parse hasn't run). Per-test handlers
	// skip individual e2e cases under -short.
	dir, err := os.MkdirTemp("", "roost-cli-e2e-")
	if err != nil {
		fmt.Fprintf(os.Stderr, "TestMain: mktemp: %v\n", err)
		os.Exit(2)
	}
	defer os.RemoveAll(dir)

	bin := filepath.Join(dir, "roost-cli")
	cmd := exec.Command("go", "build", "-o", bin, "./")
	cmd.Stderr = &bytes.Buffer{}
	if err := cmd.Run(); err != nil {
		fmt.Fprintf(os.Stderr, "TestMain: go build: %v\n%s\n", err, cmd.Stderr.(*bytes.Buffer).String())
		os.Exit(2)
	}
	builtBinaryPath = bin
	os.Exit(m.Run())
}

func builtBinary(t *testing.T) string {
	t.Helper()
	if builtBinaryPath == "" {
		t.Skip("e2e binary not built (likely -short mode)")
	}
	return builtBinaryPath
}

// runHookE2E execs the built binary with the given args and env. The
// hook fast-path guarantees exit 0 / `{}\n` stdout regardless of
// flags or args, so this test asserts the wire-level behavior — not
// IPC effects (those live in claude_hook_test.go).
func runHookE2E(t *testing.T, args []string, env map[string]string) (stdout, stderr string, exitCode int) {
	t.Helper()
	cmd := exec.Command(builtBinary(t), args...)
	cmd.Stdin = strings.NewReader("{}")
	var so, se bytes.Buffer
	cmd.Stdout = &so
	cmd.Stderr = &se
	// Start with an empty env so we don't inherit ROOST_DEBUG / ROOST_TAB_ID
	// from the test runner's shell.
	cmd.Env = []string{"PATH=/usr/bin:/bin"}
	for k, v := range env {
		cmd.Env = append(cmd.Env, k+"="+v)
	}
	err := cmd.Run()
	if err != nil {
		if exitErr, ok := err.(*exec.ExitError); ok {
			exitCode = exitErr.ExitCode()
		} else {
			t.Fatalf("unexpected exec error: %v", err)
		}
	}
	return so.String(), se.String(), exitCode
}

func TestHookE2ENominalEvent(t *testing.T) {
	stdout, stderr, ec := runHookE2E(t,
		[]string{"claude", "hook", "session-start"},
		map[string]string{
			"ROOST_TAB_ID": "999",
			"ROOST_SOCKET": "/tmp/no-such-socket-roost-cli-test",
		})
	if ec != 0 {
		t.Errorf("exit code = %d, want 0; stderr=%q", ec, stderr)
	}
	if stdout != "{}\n" {
		t.Errorf("stdout = %q, want \"{}\\n\"", stdout)
	}
	if stderr != "" {
		t.Errorf("stderr = %q, want empty", stderr)
	}
}

func TestHookE2EMalformedFlagsStillExit0(t *testing.T) {
	// Cobra would normally error on --bogus-flag. The fast-path bypasses
	// cobra entirely, so this MUST still exit 0 with `{}` on stdout.
	stdout, stderr, ec := runHookE2E(t,
		[]string{"claude", "hook", "session-start", "--bogus-flag", "value"},
		map[string]string{
			"ROOST_TAB_ID": "999",
			"ROOST_SOCKET": "/tmp/no-such-socket-roost-cli-test",
		})
	if ec != 0 {
		t.Errorf("exit code = %d, want 0; stderr=%q", ec, stderr)
	}
	if stdout != "{}\n" {
		t.Errorf("stdout = %q, want \"{}\\n\"", stdout)
	}
	if stderr != "" {
		t.Errorf("stderr = %q, want empty", stderr)
	}
}

func TestHookE2ERootFlagsBeforeSubcommand(t *testing.T) {
	// Detect-and-skip root flags before the `claude hook` subcommand.
	// This shape is plausible if a future install command writes
	// `<bin> --socket /custom claude hook session-start`.
	stdout, _, ec := runHookE2E(t,
		[]string{"--socket", "/tmp/none", "claude", "hook", "session-start"},
		map[string]string{"ROOST_TAB_ID": "999"})
	if ec != 0 {
		t.Errorf("exit code = %d, want 0", ec)
	}
	if stdout != "{}\n" {
		t.Errorf("stdout = %q, want \"{}\\n\"", stdout)
	}
}

func TestHookE2EHelpRoutesThroughCobra(t *testing.T) {
	// --help must NOT be silently swallowed by the fast-path. Cobra
	// renders its usage text, exit code is 0.
	stdout, stderr, ec := runHookE2E(t,
		[]string{"claude", "hook", "--help"},
		nil)
	if ec != 0 {
		t.Errorf("exit code = %d, want 0", ec)
	}
	combined := stdout + stderr
	if !strings.Contains(combined, "Usage:") {
		t.Errorf("expected cobra help text; combined output: %s", combined)
	}
}

func TestHookE2EMissingEvent(t *testing.T) {
	stdout, stderr, ec := runHookE2E(t,
		[]string{"claude", "hook"},
		map[string]string{"ROOST_TAB_ID": "999"})
	if ec != 0 {
		t.Errorf("exit code = %d, want 0", ec)
	}
	if stdout != "{}\n" {
		t.Errorf("stdout = %q, want \"{}\\n\"", stdout)
	}
	if stderr != "" {
		t.Errorf("stderr = %q, want empty", stderr)
	}
}

func TestHookE2EBogusEvent(t *testing.T) {
	stdout, _, ec := runHookE2E(t,
		[]string{"claude", "hook", "this-is-not-a-real-event"},
		map[string]string{"ROOST_TAB_ID": "999"})
	if ec != 0 {
		t.Errorf("exit code = %d, want 0", ec)
	}
	if stdout != "{}\n" {
		t.Errorf("stdout = %q, want \"{}\\n\"", stdout)
	}
}

func TestHookE2EDebugEnvWritesStderr(t *testing.T) {
	// With ROOST_DEBUG set and an unreachable socket, hook_debug fires
	// at least once (the socket dial failure logs). stderr is allowed
	// to be non-empty in this mode; stdout still exactly `{}\n`.
	stdout, stderr, ec := runHookE2E(t,
		[]string{"claude", "hook", "session-start"},
		map[string]string{
			"ROOST_TAB_ID": "999",
			"ROOST_SOCKET": "/tmp/no-such-socket-roost-cli-test",
			"ROOST_DEBUG":  "1",
		})
	if ec != 0 {
		t.Errorf("exit code = %d, want 0", ec)
	}
	if stdout != "{}\n" {
		t.Errorf("stdout = %q, want \"{}\\n\"", stdout)
	}
	if stderr == "" {
		t.Errorf("expected non-empty stderr in ROOST_DEBUG mode")
	}
}

func TestHookE2EUnsetTabIDIsSilent(t *testing.T) {
	// ROOST_TAB_ID empty: the hook is outside any Roost tab. Expected
	// behavior: silent no-op, exit 0, `{}\n`, no IPC, no stderr.
	stdout, stderr, ec := runHookE2E(t,
		[]string{"claude", "hook", "session-start"},
		nil)
	if ec != 0 {
		t.Errorf("exit code = %d, want 0", ec)
	}
	if stdout != "{}\n" {
		t.Errorf("stdout = %q, want \"{}\\n\"", stdout)
	}
	if stderr != "" {
		t.Errorf("stderr = %q, want empty", stderr)
	}
}
