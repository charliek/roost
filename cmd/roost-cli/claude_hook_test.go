package main

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/charliek/roost/internal/ipc"
)

// captureHandler records calls so tests can assert claude-hook
// produced the right IPC sequence.
type captureHandler struct {
	mu    sync.Mutex
	calls []string
}

func (h *captureHandler) record(method string, params any) {
	h.mu.Lock()
	defer h.mu.Unlock()
	body, _ := json.Marshal(params)
	h.calls = append(h.calls, method+" "+string(body))
}

func (h *captureHandler) snapshot() []string {
	h.mu.Lock()
	defer h.mu.Unlock()
	out := make([]string, len(h.calls))
	copy(out, h.calls)
	return out
}

func (h *captureHandler) Notify(tab int64, title, body string) error {
	h.record(ipc.MethodNotificationCreate, ipc.NotifyParams{TabID: tab, Title: title, Body: body})
	return nil
}
func (h *captureHandler) SetTitle(tab int64, title string) error { return nil }
func (h *captureHandler) Identify() ipc.Identity                 { return ipc.Identity{} }
func (h *captureHandler) FocusTab(tab int64) (ipc.TabFocusResult, error) {
	return ipc.TabFocusResult{}, nil
}
func (h *captureHandler) ListTabs() (ipc.TabListResult, error) { return ipc.TabListResult{}, nil }
func (h *captureHandler) SetTabState(tab int64, state string) error {
	h.record(ipc.MethodTabSetState, ipc.SetStateParams{TabID: tab, State: state})
	return nil
}
func (h *captureHandler) ClearTabNotification(tab int64) error {
	h.record(ipc.MethodTabClearNotification, ipc.ClearNotificationParams{TabID: tab})
	return nil
}
func (h *captureHandler) SetHookActive(tab int64, active bool) error {
	h.record(ipc.MethodSystemSetHookActive, ipc.SetHookActiveParams{TabID: tab, Active: active})
	return nil
}

// startFakeServer spins up a real ipc.Server bound to a temp socket
// and points $ROOST_SOCKET at it. Cleans up via t.Cleanup.
//
// Uses a short fixed prefix in os.TempDir() because macOS caps
// sun_path at 104 bytes — t.TempDir() with long test names would
// overrun on darwin.
func startFakeServer(t *testing.T) *captureHandler {
	t.Helper()
	h := &captureHandler{}
	dir, err := os.MkdirTemp(os.TempDir(), "rs")
	if err != nil {
		t.Fatalf("mktemp: %v", err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(dir) })
	sock := filepath.Join(dir, "s")
	s := ipc.NewServer(sock, h)
	if err := s.Start(); err != nil {
		t.Fatalf("server start: %v", err)
	}
	t.Cleanup(func() { _ = s.Close() })
	t.Setenv("ROOST_SOCKET", sock)
	return h
}

func TestClaudeHookSessionStartSetsHookActive(t *testing.T) {
	h := startFakeServer(t)
	t.Setenv("ROOST_TAB_ID", "7")

	withStdin(t, "{}", func() {
		cmdClaudeHook([]string{"session-start"})
	})
	waitForCalls(t, h, 1)

	calls := h.snapshot()
	if !strings.Contains(calls[0], "system.set_hook_active") || !strings.Contains(calls[0], `"active":true`) {
		t.Fatalf("session-start calls: %v", calls)
	}
}

func TestClaudeHookPromptSubmitClearsAndRuns(t *testing.T) {
	h := startFakeServer(t)
	t.Setenv("ROOST_TAB_ID", "5")

	withStdin(t, "{}", func() {
		cmdClaudeHook([]string{"prompt-submit"})
	})
	waitForCalls(t, h, 2)

	calls := h.snapshot()
	if !strings.Contains(calls[0], "tab.clear_notification") {
		t.Fatalf("prompt-submit first call: %s", calls[0])
	}
	if !strings.Contains(calls[1], "tab.set_state") || !strings.Contains(calls[1], `"state":"running"`) {
		t.Fatalf("prompt-submit second call: %s", calls[1])
	}
}

func TestClaudeHookNotificationFiresStateAndBanner(t *testing.T) {
	h := startFakeServer(t)
	t.Setenv("ROOST_TAB_ID", "9")

	withStdin(t, `{"message":"need permission to write"}`, func() {
		cmdClaudeHook([]string{"notification"})
	})
	waitForCalls(t, h, 2)

	calls := h.snapshot()
	if !strings.Contains(calls[0], `"state":"needs_input"`) {
		t.Fatalf("notification state: %s", calls[0])
	}
	if !strings.Contains(calls[1], "notification.create") || !strings.Contains(calls[1], "need permission to write") {
		t.Fatalf("notification banner: %s", calls[1])
	}
}

func TestClaudeHookStopSetsIdleAndBanner(t *testing.T) {
	h := startFakeServer(t)
	t.Setenv("ROOST_TAB_ID", "3")

	withStdin(t, "{}", func() {
		cmdClaudeHook([]string{"stop"})
	})
	waitForCalls(t, h, 2)

	calls := h.snapshot()
	if !strings.Contains(calls[0], `"state":"idle"`) {
		t.Fatalf("stop state: %s", calls[0])
	}
	if !strings.Contains(calls[1], "notification.create") {
		t.Fatalf("stop banner: %s", calls[1])
	}
}

func TestClaudeHookSessionEndCleansUp(t *testing.T) {
	h := startFakeServer(t)
	t.Setenv("ROOST_TAB_ID", "11")

	withStdin(t, "{}", func() {
		cmdClaudeHook([]string{"session-end"})
	})
	waitForCalls(t, h, 3)

	calls := h.snapshot()
	if !strings.Contains(calls[0], `"active":false`) {
		t.Fatalf("session-end first call: %s", calls[0])
	}
	if !strings.Contains(calls[1], `"state":"none"`) {
		t.Fatalf("session-end state: %s", calls[1])
	}
	if !strings.Contains(calls[2], "tab.clear_notification") {
		t.Fatalf("session-end clear: %s", calls[2])
	}
}

func TestClaudeHookMissingTabIDIsSilentNoOp(t *testing.T) {
	h := startFakeServer(t)
	t.Setenv("ROOST_TAB_ID", "")

	withStdin(t, "{}", func() {
		cmdClaudeHook([]string{"notification"})
	})
	// Wait briefly to ensure no calls land.
	time.Sleep(150 * time.Millisecond)
	if len(h.snapshot()) != 0 {
		t.Fatalf("expected no IPC calls without ROOST_TAB_ID, got %v", h.snapshot())
	}
}

// --- helpers --------------------------------------------------------

// withStdin redirects stdin for the duration of fn so cmdClaudeHook
// reads the supplied payload. fn is expected to be synchronous.
func withStdin(t *testing.T, body string, fn func()) {
	t.Helper()
	r, w, err := os.Pipe()
	if err != nil {
		t.Fatalf("pipe: %v", err)
	}
	orig := os.Stdin
	os.Stdin = r
	t.Cleanup(func() { os.Stdin = orig })

	_, _ = w.WriteString(body)
	_ = w.Close()

	// Suppress the {} stdout output from the hook.
	devNull, _ := os.OpenFile(os.DevNull, os.O_WRONLY, 0)
	if devNull != nil {
		oldStdout := os.Stdout
		os.Stdout = devNull
		t.Cleanup(func() {
			os.Stdout = oldStdout
			_ = devNull.Close()
		})
	}
	fn()
}

// waitForCalls polls until at least n calls have been recorded or
// the deadline passes.
func waitForCalls(t *testing.T, h *captureHandler, n int) {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	for {
		if len(h.snapshot()) >= n {
			return
		}
		if ctx.Err() != nil {
			t.Fatalf("waited for %d calls; got %d (%v)", n, len(h.snapshot()), h.snapshot())
		}
		time.Sleep(20 * time.Millisecond)
	}
}
