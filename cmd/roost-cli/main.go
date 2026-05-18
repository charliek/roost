// Command roost-cli is the companion CLI for the Roost GUI app. It
// talks to the running app over the Unix socket exposed by the GUI's
// internal/ipc server.
//
// Surface:
//
//	roost-cli notify TITLE [BODY] [--tab ID]
//	roost-cli identify
//	roost-cli tab list
//	roost-cli tab focus [TAB_ID]
//	roost-cli tab set-title TITLE [--tab ID]
//	roost-cli tab set-state STATE [--tab ID]
//	roost-cli claude install [--force]
//	roost-cli claude hook EVENT       (reads JSON on stdin)
//	roost-cli version
//	roost-cli completion bash|zsh|fish|powershell
//
// Persistent flags: --socket, --json, --timeout, -v.
//
// `claude hook` is special: see the hook fast-path in main() and the
// XXX comments on runClaudeHook.
package main

import (
	"errors"
	"fmt"
	"os"

	"github.com/spf13/pflag"
)

func main() {
	// HOOK FAST-PATH: detect `claude hook` invocations and handle them
	// before cobra ever sees the args. This guarantees the strict
	// invariants (always exit 0, always emit `{}`, suppress all
	// errors) regardless of what flags Claude passes — cobra would
	// reject unknown flags during parsing and break the contract.
	//
	// We fall through to cobra only when --help/-h is present so help
	// works, and when the args don't match the hook shape.
	if hookArgs, ok := detectHookFastPath(os.Args[1:]); ok {
		runClaudeHook(hookArgs)
		os.Exit(0)
	}

	if err := rootCmd.Execute(); err != nil {
		// Cobra has SilenceErrors=true, so we own the rendering.
		// Classify the error to pick the right exit code:
		//   - help requested → 0
		//   - usage / parse error → 2
		//   - runtime error → 1
		switch {
		case errors.Is(err, pflag.ErrHelp):
			os.Exit(0)
		case errors.Is(err, errUsage):
			if clientCtx.JSON {
				_ = outputError(err)
			} else {
				fmt.Fprintf(os.Stderr, "roost-cli: %v\n", err)
			}
			os.Exit(2)
		default:
			if clientCtx.JSON {
				_ = outputError(err)
			} else {
				fmt.Fprintf(os.Stderr, "roost-cli: %v\n", err)
			}
			os.Exit(1)
		}
	}
}

// detectHookFastPath scans args (NOT including os.Args[0]) for the
// `claude hook` shape, walking past any optional root flags that
// might appear before the subcommand.
//
// Returns (hook event args, true) on a match, ([], false) otherwise.
//
// Falls through to cobra (returns false) when --help or -h appears
// anywhere in the original args, so `roost-cli claude hook --help`
// produces help instead of the silent {} output.
func detectHookFastPath(args []string) ([]string, bool) {
	for _, a := range args {
		if a == "--help" || a == "-h" {
			return nil, false
		}
	}

	i := 0
	// Skip optional root flags. Keep this list in sync with root.go's
	// PersistentFlags. Unknown tokens here cause us to abort the
	// fast-path and fall through to cobra, which will error properly.
	for i < len(args) {
		switch args[i] {
		case "--json":
			i++
		case "--socket", "--timeout":
			// "--name VALUE" form; need at least one more arg.
			if i+1 >= len(args) {
				return nil, false
			}
			i += 2
		case "-v", "-vv", "-vvv", "--verbose":
			i++
		default:
			// "--name=VALUE" form for --socket / --timeout.
			if hasFlagPrefix(args[i], "--socket=") || hasFlagPrefix(args[i], "--timeout=") {
				i++
				continue
			}
			// Anything else: hopefully the subcommand. Stop scanning.
			goto done
		}
	}
done:

	if i+1 >= len(args) {
		return nil, false
	}
	if args[i] != "claude" || args[i+1] != "hook" {
		return nil, false
	}
	return args[i+2:], true
}

func hasFlagPrefix(arg, prefix string) bool {
	return len(arg) >= len(prefix) && arg[:len(prefix)] == prefix
}
