package main

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"time"

	"github.com/charliek/roost/internal/config"
	"github.com/charliek/roost/internal/ipc"
	"github.com/spf13/cobra"
)

// claudeHookCmd is registered with cobra so that `roost-cli claude
// hook --help` and shell completion work. Production invocations
// never reach this command's RunE — main()'s fast-path bypasses cobra
// entirely for `claude hook EVENT`. See main.go for the rationale.
//
// XXX: do not delete the fast-path in main() and route through this
// command's RunE alone — cobra's flag parser would reject unknown
// flags Claude might add to its hook invocations, breaking the strict
// "always exit 0" invariant.
var claudeHookCmd = &cobra.Command{
	Use:   "hook EVENT",
	Short: "Hook handler for Claude Code (invoked via the install-generated settings)",
	Long: `Hook handler invoked by Claude Code with the event name and a JSON
payload on stdin. Used to surface Claude session state into the Roost
indicator and notification badges.

This command is not normally run by hand — 'roost-cli claude install'
wires it into Claude's settings file. Always exits 0 with '{}' on
stdout, regardless of whether the GUI is reachable.`,
	Args:          cobra.ArbitraryArgs,
	SilenceErrors: true,
	SilenceUsage:  true,
	RunE: func(cmd *cobra.Command, args []string) error {
		runClaudeHook(args)
		return nil
	},
}

// runClaudeHook executes the hook handler with the strict invariants:
//
//   - Always emit '{}' on os.Stdout (via emitClaudeHookOutput).
//   - Always swallow errors. Anything that goes wrong is debug-logged
//     to stderr only when ROOST_DEBUG is set (or -v was passed).
//   - Recover panics so a future contributor's typed-decode
//     experiment can't regress the contract silently.
//
// Called from two places:
//  1. main()'s fast-path, which detects `claude hook` in os.Args and
//     bypasses cobra entirely. This is the production path.
//  2. claudeHookCmd.RunE, which exists only so --help and completion
//     work. Defense-in-depth.
//
// XXX: do NOT replace the os.Stdout writes here with cmd.OutOrStdout()
// or any cobra-redirectable writer — Claude reads the hook's stdout
// directly via the spawned pipe and a redirected stream would never
// reach it.
func runClaudeHook(args []string) {
	defer func() {
		// Recover any panic so we still emit `{}` and exit 0. Without
		// this a future typed-decode change could crash the hook and
		// surface a stack trace to the user via Claude.
		if r := recover(); r != nil {
			hookDebug("panic: %v", r)
			emitClaudeHookOutput()
		}
	}()

	if len(args) == 0 {
		emitClaudeHookOutput()
		return
	}
	event := args[0]

	tabID := tabIDFromEnv()
	if tabID == 0 {
		// Outside a Roost tab — silent no-op.
		emitClaudeHookOutput()
		return
	}

	// Read the hook payload off stdin into a bounded buffer. We don't
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
}

// claudeHookMessage extracts a human-readable body from the hook
// payload. Claude's Notification event has a "message" field; other
// shapes fall back to the static default.
func claudeHookMessage(parsed map[string]any, fallback string) string {
	if parsed == nil {
		return fallback
	}
	if v, ok := parsed["message"].(string); ok && v != "" {
		return v
	}
	return fallback
}

// sendHookCall is a fire-and-forget IPC helper used only by the hook
// path. A failed call means the GUI isn't running or the socket
// changed — neither is worth surfacing back to Claude.
//
// Intentionally does NOT use the IPCClient wrapper — clientCtx is
// only populated by PersistentPreRunE, and the hook fast-path
// bypasses cobra (so PersistentPreRunE never runs in production).
// Soft-resolve the socket here directly.
func sendHookCall(method string, params any) error {
	body, err := json.Marshal(params)
	if err != nil {
		return err
	}
	socket, err := lookupHookSocket()
	if err != nil {
		hookDebug("socket: %v", err)
		return err
	}
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

// lookupHookSocket is the soft-fail socket resolver for the hook
// path. Returns an error rather than calling os.Exit so a missing
// runtime dir doesn't leak a non-zero exit out of the hook.
//
// Mirrors the pre-cobra rationale documented in the older main.go:
// the user-facing CLI verbs (notify, identify, tab.*) want a fatal
// resolve so misconfiguration surfaces; the hook does not.
func lookupHookSocket() (string, error) {
	if v := os.Getenv("ROOST_SOCKET"); v != "" {
		return v, nil
	}
	paths, err := config.Resolve()
	if err != nil {
		return "", err
	}
	return paths.SocketPath(), nil
}

// hookDebug writes a stderr line only when debug mode is on (either
// ROOST_DEBUG env or -v flag). Otherwise silent — Claude prints hook
// stderr to the user.
func hookDebug(format string, args ...any) {
	if !debugEnabled() {
		return
	}
	fmt.Fprintf(os.Stderr, "roost claude hook: "+format+"\n", args...)
}

// emitClaudeHookOutput writes the empty hook-result payload Claude
// expects on stdout. ALWAYS writes to os.Stdout directly — see the
// XXX comment on runClaudeHook.
func emitClaudeHookOutput() {
	fmt.Fprintln(os.Stdout, "{}")
}
