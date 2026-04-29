// Command roost-cli is the companion CLI for the Roost GUI app. It
// talks to the running app over the Unix socket exposed by the GUI's
// internal/ipc server. Intended to be invoked from inside a Roost tab
// (typically by Claude Code hooks).
//
// Usage:
//
//	roost-cli notify --title "Build done" [--body "..."] [--tab <id>]
//	roost-cli set-title "my-tab"
//	roost-cli identify
//
// Tab id falls back to $ROOST_TAB_ID when --tab is not given. Socket
// path falls back to $ROOST_SOCKET, then the platform default.
package main

import (
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"strconv"
	"time"

	"github.com/charliek/roost/internal/config"
	"github.com/charliek/roost/internal/ipc"
)

func main() {
	if len(os.Args) < 2 {
		usage(os.Stderr)
		os.Exit(2)
	}

	switch os.Args[1] {
	case "notify":
		os.Exit(cmdNotify(os.Args[2:]))
	case "set-title":
		os.Exit(cmdSetTitle(os.Args[2:]))
	case "identify":
		os.Exit(cmdIdentify(os.Args[2:]))
	case "tab":
		os.Exit(cmdTab(os.Args[2:]))
	case "claude-hook":
		os.Exit(cmdClaudeHook(os.Args[2:]))
	case "claude":
		os.Exit(cmdClaude(os.Args[2:]))
	case "-h", "--help", "help":
		usage(os.Stdout)
		os.Exit(0)
	default:
		fmt.Fprintf(os.Stderr, "roost: unknown command %q\n\n", os.Args[1])
		usage(os.Stderr)
		os.Exit(2)
	}
}

func usage(w *os.File) {
	fmt.Fprintln(w, "usage:")
	fmt.Fprintln(w, "  roost-cli notify --title TITLE [--body BODY] [--tab ID]")
	fmt.Fprintln(w, "  roost-cli set-title --title TITLE [--tab ID]")
	fmt.Fprintln(w, "  roost-cli identify")
	fmt.Fprintln(w, "  roost-cli tab focus [--tab ID]")
	fmt.Fprintln(w, "  roost-cli tab list [--json]")
	fmt.Fprintln(w, "  roost-cli tab set-state --state STATE [--tab ID]")
	fmt.Fprintln(w, "  roost-cli claude install [--force]")
	fmt.Fprintln(w, "  roost-cli claude-hook EVENT     (reads JSON from stdin)")
}

func cmdNotify(args []string) int {
	fs := flag.NewFlagSet("notify", flag.ContinueOnError)
	title := fs.String("title", "", "notification title (required)")
	body := fs.String("body", "", "notification body")
	tab := fs.Int64("tab", tabIDFromEnv(), "tab id (defaults to $ROOST_TAB_ID)")
	if err := fs.Parse(args); err != nil {
		return 2
	}
	if *title == "" {
		fmt.Fprintln(os.Stderr, "roost notify: --title is required")
		return 2
	}
	params, _ := json.Marshal(ipc.NotifyParams{TabID: *tab, Title: *title, Body: *body})
	return doRPC(ipc.MethodNotificationCreate, params)
}

func cmdSetTitle(args []string) int {
	fs := flag.NewFlagSet("set-title", flag.ContinueOnError)
	title := fs.String("title", "", "new tab title (required)")
	tab := fs.Int64("tab", tabIDFromEnv(), "tab id (defaults to $ROOST_TAB_ID)")
	if err := fs.Parse(args); err != nil {
		return 2
	}
	if *title == "" && len(fs.Args()) > 0 {
		*title = fs.Arg(0) // allow positional shorthand: roost set-title "Foo"
	}
	if *title == "" {
		fmt.Fprintln(os.Stderr, "roost set-title: title is required (use --title or pass positionally)")
		return 2
	}
	params, _ := json.Marshal(ipc.SetTitleParams{TabID: *tab, Title: *title})
	return doRPC(ipc.MethodTabSetTitle, params)
}

func cmdIdentify(_ []string) int {
	return doRPC(ipc.MethodSystemIdentify, json.RawMessage(`{}`))
}

func doRPC(method string, params json.RawMessage) int {
	socket := socketPath()
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()

	resp, err := ipc.Dial(ctx, socket, ipc.Request{
		ID:     "1",
		Method: method,
		Params: params,
	})
	if err != nil {
		fmt.Fprintf(os.Stderr, "roost: connect %s: %v\n", socket, err)
		return 1
	}
	if !resp.OK {
		if resp.Error != nil {
			fmt.Fprintf(os.Stderr, "roost: %s: %s\n", resp.Error.Code, resp.Error.Message)
		} else {
			fmt.Fprintln(os.Stderr, "roost: request failed")
		}
		return 1
	}
	if resp.Result != nil {
		out, _ := json.MarshalIndent(resp.Result, "", "  ")
		fmt.Println(string(out))
	}
	return 0
}

func tabIDFromEnv() int64 {
	v := os.Getenv("ROOST_TAB_ID")
	if v == "" {
		return 0
	}
	id, err := strconv.ParseInt(v, 10, 64)
	if err != nil {
		return 0
	}
	return id
}

// socketPath resolves the socket address: $ROOST_SOCKET wins; otherwise
// the platform-default path under config.Paths.RuntimeDir.
func socketPath() string {
	if v := os.Getenv("ROOST_SOCKET"); v != "" {
		return v
	}
	p, err := config.Resolve()
	if err != nil {
		fmt.Fprintf(os.Stderr, "roost: config.Resolve: %v\n", err)
		os.Exit(1)
	}
	return p.SocketPath()
}
