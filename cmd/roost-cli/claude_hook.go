package main

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"time"

	"github.com/charliek/roost/internal/ipc"
)

// cmdClaudeHook handles `roost-cli claude-hook EVENT`. Wired into
// Claude Code via the generated settings file (`roost-cli claude
// install`); each hook event Claude fires invokes us with the event
// name and a JSON payload on stdin.
//
// Two strict invariants:
//
//   - Always exit 0. Claude treats nonzero as a failed hook and
//     surfaces an error to the user.
//   - Always print `{}` to stdout. Some Claude versions parse hook
//     output; an empty payload means "do nothing extra."
//
// Anything that goes wrong (bad env, no GUI running, IPC failure)
// is logged to stderr (only when ROOST_DEBUG is set) and swallowed.
func cmdClaudeHook(args []string) int {
	if len(args) == 0 {
		emitClaudeHookOutput()
		return 0
	}
	event := args[0]

	tabID := tabIDFromEnv()
	if tabID == 0 {
		// Outside a Roost tab — silent no-op.
		emitClaudeHookOutput()
		return 0
	}

	// Read the hook payload off stdin to a bounded buffer. We don't
	// use most of it today, but exhaust the pipe so Claude doesn't
	// block on a closed reader.
	payload, _ := io.ReadAll(io.LimitReader(os.Stdin, 1<<20))
	var parsed map[string]any
	_ = json.Unmarshal(payload, &parsed)

	switch event {
	case "session-start":
		// Open hook session; OSC 9/777 suppression engages from this
		// point. State stays at "none" — running only appears when the
		// user actually submits a prompt.
		_ = sendHookCall(ipc.MethodSystemSetHookActive,
			ipc.SetHookActiveParams{TabID: tabID, Active: true})

	case "prompt-submit":
		_ = sendHookCall(ipc.MethodTabClearNotification,
			ipc.ClearNotificationParams{TabID: tabID})
		_ = sendHookCall(ipc.MethodTabSetState,
			ipc.SetStateParams{TabID: tabID, State: "running"})

	case "notification":
		_ = sendHookCall(ipc.MethodTabSetState,
			ipc.SetStateParams{TabID: tabID, State: "needs_input"})
		body := claudeHookMessage(parsed, "Claude needs input")
		_ = sendHookCall(ipc.MethodNotificationCreate,
			ipc.NotifyParams{TabID: tabID, Title: "Claude Code", Body: body})

	case "stop":
		_ = sendHookCall(ipc.MethodTabSetState,
			ipc.SetStateParams{TabID: tabID, State: "idle"})
		_ = sendHookCall(ipc.MethodNotificationCreate,
			ipc.NotifyParams{TabID: tabID, Title: "Claude Code", Body: "Turn complete"})

	case "session-end":
		_ = sendHookCall(ipc.MethodSystemSetHookActive,
			ipc.SetHookActiveParams{TabID: tabID, Active: false})
		_ = sendHookCall(ipc.MethodTabSetState,
			ipc.SetStateParams{TabID: tabID, State: "none"})
		_ = sendHookCall(ipc.MethodTabClearNotification,
			ipc.ClearNotificationParams{TabID: tabID})

	default:
		hookDebug("unknown event: %s", event)
	}

	emitClaudeHookOutput()
	return 0
}

// claudeHookMessage extracts a human-readable body from the hook
// payload. Claude's Notification event has a "message" field; other
// shapes fall back to a static default.
func claudeHookMessage(parsed map[string]any, fallback string) string {
	if parsed == nil {
		return fallback
	}
	if v, ok := parsed["message"].(string); ok && v != "" {
		return v
	}
	return fallback
}

// sendHookCall is a fire-and-forget IPC helper. A failed call means
// the GUI isn't running or the socket changed — neither is worth
// surfacing back to Claude.
func sendHookCall(method string, params any) error {
	body, err := json.Marshal(params)
	if err != nil {
		return err
	}
	socket := socketPath()
	ctx, cancel := context.WithTimeout(context.Background(), 1500*time.Millisecond)
	defer cancel()
	resp, err := ipc.Dial(ctx, socket, ipc.Request{
		ID: "1", Method: method, Params: body,
	})
	if err != nil {
		hookDebug("dial %s: %v", method, err)
		return err
	}
	if !resp.OK {
		if resp.Error != nil {
			hookDebug("%s: %s: %s", method, resp.Error.Code, resp.Error.Message)
		}
		return fmt.Errorf("rpc not ok")
	}
	return nil
}

// hookDebug writes a stderr line only when ROOST_DEBUG is set in env.
// Otherwise silent — Claude prints hook stderr to the user.
func hookDebug(format string, args ...any) {
	if os.Getenv("ROOST_DEBUG") == "" {
		return
	}
	fmt.Fprintf(os.Stderr, "roost claude-hook: "+format+"\n", args...)
}

// emitClaudeHookOutput writes the empty hook-result payload Claude
// expects on stdout.
func emitClaudeHookOutput() {
	fmt.Println("{}")
}
