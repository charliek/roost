package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestQuoteForShell(t *testing.T) {
	cases := []struct {
		in, want string
	}{
		{"plain", "plain"},
		{"/usr/local/bin/roost-cli", "/usr/local/bin/roost-cli"},
		{"/with space/cli", "'/with space/cli'"},
		{"/with$dollar/cli", "'/with$dollar/cli'"},
		{`/with\backslash/cli`, `'/with\backslash/cli'`},
		{`/with"quote/cli`, `'/with"quote/cli'`},
		{"/with'apostrophe/cli", `'/with'\''apostrophe/cli'`},
		{"with`backtick/cli", "'with`backtick/cli'"},
	}
	for _, c := range cases {
		if got := quoteForShell(c.in); got != c.want {
			t.Errorf("quoteForShell(%q) = %q, want %q", c.in, got, c.want)
		}
	}
}

func TestBuildClaudeSettingsShape(t *testing.T) {
	doc := buildClaudeSettings("/usr/local/bin/roost-cli")

	hooks, ok := doc["hooks"].(map[string]any)
	if !ok {
		t.Fatalf("expected hooks map, got %T", doc["hooks"])
	}

	want := map[string]string{
		"SessionStart":     "session-start",
		"UserPromptSubmit": "prompt-submit",
		"Notification":     "notification",
		"Stop":             "stop",
		"SessionEnd":       "session-end",
	}

	for key, event := range want {
		entry, ok := hooks[key].([]any)
		if !ok || len(entry) != 1 {
			t.Fatalf("hooks[%q] = %v, want one-element list", key, hooks[key])
		}
		matcher, ok := entry[0].(map[string]any)
		if !ok {
			t.Fatalf("hooks[%q][0] = %v, want map", key, entry[0])
		}
		hookList, ok := matcher["hooks"].([]any)
		if !ok || len(hookList) != 1 {
			t.Fatalf("hooks[%q] inner hooks not a one-element list", key)
		}
		hook, _ := hookList[0].(map[string]any)
		if hook["type"] != "command" {
			t.Errorf("hooks[%q] type = %v, want command", key, hook["type"])
		}
		// The generated command line uses the new "claude hook" form
		// (space, not hyphen) — this is the central regression target.
		wantCmd := "/usr/local/bin/roost-cli claude hook " + event
		if hook["command"] != wantCmd {
			t.Errorf("hooks[%q] command = %q, want %q", key, hook["command"], wantCmd)
		}
	}
}

func TestBuildClaudeSettingsQuotesPathWithSpaces(t *testing.T) {
	doc := buildClaudeSettings("/path with space/roost-cli")
	hooks := doc["hooks"].(map[string]any)
	entry := hooks["SessionStart"].([]any)[0].(map[string]any)
	hook := entry["hooks"].([]any)[0].(map[string]any)
	got := hook["command"].(string)
	want := "'/path with space/roost-cli' claude hook session-start"
	if got != want {
		t.Errorf("command with space: got %q, want %q", got, want)
	}
}

func TestClaudeInstallStdoutStderrSplit(t *testing.T) {
	// CRITICAL: the alias goes to stdout, status messages to stderr.
	// `roost-cli claude install >> ~/.bashrc` must append ONLY the
	// alias line. This test pins the split.
	tmp := t.TempDir()
	t.Setenv("XDG_CONFIG_HOME", tmp)
	t.Setenv("XDG_DATA_HOME", filepath.Join(tmp, "data"))
	t.Setenv("XDG_RUNTIME_DIR", filepath.Join(tmp, "run"))
	t.Setenv("HOME", tmp)

	prevForce := claudeInstallForce
	claudeInstallForce = true
	t.Cleanup(func() { claudeInstallForce = prevForce })

	prevJSON := clientCtx.JSON
	clientCtx.JSON = false
	t.Cleanup(func() { clientCtx.JSON = prevJSON })

	stderr := captureStderr(t, func() {
		stdout := captureStdout(t, func() {
			if err := runClaudeInstall(claudeInstallCmd, nil); err != nil {
				t.Fatalf("runClaudeInstall: %v", err)
			}
		})
		// The alias must appear on stdout. The "Wrote ..." status
		// message must NOT appear on stdout.
		if !strings.Contains(stdout, "alias claude=") {
			t.Errorf("stdout missing alias line:\n%s", stdout)
		}
		if strings.Contains(stdout, "Wrote ") {
			t.Errorf("stdout MUST NOT contain status line — '>> ~/.bashrc' would inject it. Got:\n%s", stdout)
		}
	})

	// "# Wrote ..." status must be on stderr.
	if !strings.Contains(stderr, "# Wrote ") {
		t.Errorf("stderr missing 'Wrote' status:\n%s", stderr)
	}
}

func TestClaudeInstallNoForceRefusesExistingFile(t *testing.T) {
	tmp := t.TempDir()
	t.Setenv("XDG_CONFIG_HOME", tmp)
	t.Setenv("XDG_DATA_HOME", filepath.Join(tmp, "data"))
	t.Setenv("XDG_RUNTIME_DIR", filepath.Join(tmp, "run"))
	t.Setenv("HOME", tmp)

	settingsPath := filepath.Join(tmp, "roost", "claude-settings.json")
	if err := os.MkdirAll(filepath.Dir(settingsPath), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(settingsPath, []byte(`{"existing":true}`), 0o600); err != nil {
		t.Fatal(err)
	}

	prevForce := claudeInstallForce
	claudeInstallForce = false
	t.Cleanup(func() { claudeInstallForce = prevForce })

	// Discard output; we only care about the error.
	_ = captureStderr(t, func() {
		_ = captureStdout(t, func() {
			err := runClaudeInstall(claudeInstallCmd, nil)
			if err == nil {
				t.Fatal("expected error when file exists without --force")
			}
			if !strings.Contains(err.Error(), "already exists") {
				t.Errorf("unexpected error: %v", err)
			}
		})
	})

	// Original file should be untouched.
	got, err := os.ReadFile(settingsPath)
	if err != nil {
		t.Fatal(err)
	}
	if string(got) != `{"existing":true}` {
		t.Errorf("file modified despite refusal: %s", got)
	}
}

func TestClaudeInstallForceOverwrites(t *testing.T) {
	tmp := t.TempDir()
	t.Setenv("XDG_CONFIG_HOME", tmp)
	t.Setenv("XDG_DATA_HOME", filepath.Join(tmp, "data"))
	t.Setenv("XDG_RUNTIME_DIR", filepath.Join(tmp, "run"))
	t.Setenv("HOME", tmp)

	settingsPath := filepath.Join(tmp, "roost", "claude-settings.json")
	if err := os.MkdirAll(filepath.Dir(settingsPath), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(settingsPath, []byte(`{"old":true}`), 0o600); err != nil {
		t.Fatal(err)
	}

	prevForce := claudeInstallForce
	claudeInstallForce = true
	t.Cleanup(func() { claudeInstallForce = prevForce })

	_ = captureStderr(t, func() {
		_ = captureStdout(t, func() {
			if err := runClaudeInstall(claudeInstallCmd, nil); err != nil {
				t.Fatalf("force install: %v", err)
			}
		})
	})

	got, err := os.ReadFile(settingsPath)
	if err != nil {
		t.Fatal(err)
	}
	var parsed map[string]any
	if err := json.Unmarshal(got, &parsed); err != nil {
		t.Fatalf("not JSON: %v", err)
	}
	if _, ok := parsed["hooks"]; !ok {
		t.Errorf("expected new file to contain hooks key, got %s", got)
	}
}

func TestClaudeInstallRejectsJSONMode(t *testing.T) {
	prevJSON := clientCtx.JSON
	clientCtx.JSON = true
	t.Cleanup(func() { clientCtx.JSON = prevJSON })

	err := runClaudeInstall(claudeInstallCmd, nil)
	if err == nil {
		t.Fatal("expected error when --json is set on claude install")
	}
}
