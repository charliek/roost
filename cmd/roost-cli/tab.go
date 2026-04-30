package main

import (
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"time"

	"github.com/charliek/roost/internal/ipc"
)

// cmdTab dispatches `roost-cli tab <focus|list|set-state>`.
func cmdTab(args []string) int {
	if len(args) == 0 {
		fmt.Fprintln(os.Stderr, "roost tab: subcommand required (focus|list|set-state)")
		return 2
	}
	switch args[0] {
	case "focus":
		return cmdTabFocus(args[1:])
	case "list":
		return cmdTabList(args[1:])
	case "set-state":
		return cmdTabSetState(args[1:])
	default:
		fmt.Fprintf(os.Stderr, "roost tab: unknown subcommand %q\n", args[0])
		return 2
	}
}

// cmdTabFocus calls tab.focus, switching the GUI to the named tab and
// raising the window. Used by terminal-notifier's -execute callback
// and by scripts ("jump to the build tab").
func cmdTabFocus(args []string) int {
	fs := flag.NewFlagSet("tab focus", flag.ContinueOnError)
	tab := fs.Int64("tab", tabIDFromEnv(), "tab id (defaults to $ROOST_TAB_ID)")
	if err := fs.Parse(args); err != nil {
		return 2
	}
	if *tab == 0 {
		fmt.Fprintln(os.Stderr, "roost tab focus: --tab required (or set $ROOST_TAB_ID)")
		return 2
	}
	params, _ := json.Marshal(ipc.TabFocusParams{TabID: *tab})
	return doRPC(ipc.MethodTabFocus, params)
}

// cmdTabList queries tab.list and renders the result. Default output
// is a project-grouped tree; --json prints the raw IPC result.
func cmdTabList(args []string) int {
	fs := flag.NewFlagSet("tab list", flag.ContinueOnError)
	asJSON := fs.Bool("json", false, "print raw JSON response")
	if err := fs.Parse(args); err != nil {
		return 2
	}

	socket := socketPath()
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	resp, err := ipc.Dial(ctx, socket, ipc.Request{
		ID: "1", Method: ipc.MethodTabList, Params: json.RawMessage(`{}`),
	})
	if err != nil {
		fmt.Fprintf(os.Stderr, "roost: connect %s: %v\n", socket, err)
		return 1
	}
	if !resp.OK {
		if resp.Error != nil {
			fmt.Fprintf(os.Stderr, "roost: %s: %s\n", resp.Error.Code, resp.Error.Message)
		}
		return 1
	}
	if *asJSON {
		out, _ := json.MarshalIndent(resp.Result, "", "  ")
		fmt.Println(string(out))
		return 0
	}
	return printTabTree(resp.Result)
}

// printTabTree pretty-prints the project-grouped tree for humans.
// Avoids re-decoding to a typed struct to keep the CLI dependency
// surface small (resp.Result is already map[string]any from the
// server roundtrip).
func printTabTree(result any) int {
	m, ok := result.(map[string]any)
	if !ok {
		fmt.Println(result)
		return 0
	}
	projects, _ := m["projects"].([]any)
	for _, p := range projects {
		pm, _ := p.(map[string]any)
		name, _ := pm["name"].(string)
		fmt.Printf("%s (id=%v)\n", name, pm["id"])
		tabs, _ := pm["tabs"].([]any)
		for _, tab := range tabs {
			tm, _ := tab.(map[string]any)
			marker := "  "
			if active, _ := tm["is_active"].(bool); active {
				marker = "* "
			}
			notif := ""
			if hn, _ := tm["has_notification"].(bool); hn {
				notif = " [!]"
			}
			state, _ := tm["agent_state"].(string)
			if state == "" || state == "none" {
				state = "-"
			}
			title, _ := tm["title"].(string)
			fmt.Printf("%s[%v] %s  state=%s%s\n", marker, tm["id"], title, state, notif)
		}
	}
	return 0
}

// cmdTabSetState exposes tab.set_state for non-Claude agents (or
// scripts) that want to drive the indicator without going through
// claude-hook. Validation lives server-side.
func cmdTabSetState(args []string) int {
	fs := flag.NewFlagSet("tab set-state", flag.ContinueOnError)
	tab := fs.Int64("tab", tabIDFromEnv(), "tab id (defaults to $ROOST_TAB_ID)")
	state := fs.String("state", "", "new agent state (none|running|needs_input|idle)")
	if err := fs.Parse(args); err != nil {
		return 2
	}
	if *tab == 0 {
		fmt.Fprintln(os.Stderr, "roost tab set-state: --tab required (or set $ROOST_TAB_ID)")
		return 2
	}
	if *state == "" {
		fmt.Fprintln(os.Stderr, "roost tab set-state: --state required")
		return 2
	}
	params, _ := json.Marshal(ipc.SetStateParams{TabID: *tab, State: *state})
	return doRPC(ipc.MethodTabSetState, params)
}
