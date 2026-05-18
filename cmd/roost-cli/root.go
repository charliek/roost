package main

import (
	"errors"
	"fmt"
	"os"
	"strconv"
	"time"

	"github.com/charliek/roost/internal/config"
	"github.com/spf13/cobra"
)

// Persistent flag values, populated by cobra. Package-level because
// cobra's RunE callbacks can't accept extra args.
var (
	flagSocket  string
	flagJSON    bool
	flagTimeout time.Duration
	flagVerbose int
)

// clientCtx holds resolved per-invocation state. Populated by
// PersistentPreRunE so commands don't re-resolve on every call.
type clientCtxT struct {
	SocketPath string
	Timeout    time.Duration
	JSON       bool
}

var clientCtx clientCtxT

// errUsage marks errors that should produce exit code 2 (usage error)
// rather than 1 (runtime error). RunE bodies wrap argument-validation
// failures in this so main() can classify them.
var errUsage = errors.New("usage")

// errUsageMsg wraps a message with the errUsage sentinel.
func errUsageMsg(format string, args ...any) error {
	return fmt.Errorf("%w: "+format, append([]any{errUsage}, args...)...)
}

// preRunSkip is the set of commands that do not talk to IPC and must
// NOT have PersistentPreRunE attempt to resolve a socket path. On a
// fresh machine the runtime dir might not exist; failing here would
// break completion-script generation, version printing, etc.
var preRunSkip = map[string]bool{
	"roost-cli version":               true,
	"roost-cli completion":            true,
	"roost-cli help":                  true,
	"roost-cli claude install":        true,
	"roost-cli claude hook":           true, // cobra only sees `claude hook` for --help; production bypasses cobra
	"roost-cli completion bash":       true,
	"roost-cli completion zsh":        true,
	"roost-cli completion fish":       true,
	"roost-cli completion powershell": true,
}

var rootCmd = &cobra.Command{
	Use:   "roost-cli",
	Short: "Companion CLI for the Roost terminal multiplexer",
	Long: `roost-cli talks to a running Roost GUI over a Unix socket.

It is intended to be invoked from inside a Roost tab (typically by Claude
Code hooks) but the surface is general — any script can drive Roost from
the command line.

Most subcommands talk to the GUI; if no GUI is running they fail with a
hint. Run 'roost-cli claude install' to wire Claude Code hooks; run
'roost-cli completion zsh' (or bash/fish/powershell) to install shell
completions.`,
	SilenceUsage:  true,
	SilenceErrors: true,
	PersistentPreRunE: func(cmd *cobra.Command, args []string) error {
		clientCtx.JSON = flagJSON
		clientCtx.Timeout = flagTimeout

		if preRunSkip[cmd.CommandPath()] {
			return nil
		}

		// Socket precedence: explicit --socket flag > $ROOST_SOCKET env
		// > config-resolved default. The "lookup" name mirrors the soft
		// resolver used by claude_hook.go so the rationale stays in
		// one place: callers in the hook path need a non-fatal
		// resolution path. Other commands surface the error normally.
		if flagSocket != "" {
			clientCtx.SocketPath = flagSocket
			return nil
		}
		if v := os.Getenv("ROOST_SOCKET"); v != "" {
			clientCtx.SocketPath = v
			return nil
		}
		paths, err := config.Resolve()
		if err != nil {
			return fmt.Errorf("resolve socket path: %w", err)
		}
		clientCtx.SocketPath = paths.SocketPath()
		return nil
	},
}

func init() {
	rootCmd.PersistentFlags().StringVar(&flagSocket, "socket", "", "Path to the Roost IPC socket (overrides $ROOST_SOCKET)")
	rootCmd.PersistentFlags().BoolVar(&flagJSON, "json", false, "Emit machine-readable JSON output where applicable")
	rootCmd.PersistentFlags().DurationVar(&flagTimeout, "timeout", 3*time.Second, "IPC dial+request timeout")
	rootCmd.PersistentFlags().CountVarP(&flagVerbose, "verbose", "v", "Increase verbosity (-v, -vv); same effect as ROOST_DEBUG")
}

// tabIDFromEnv returns the tab id from $ROOST_TAB_ID, or 0 when unset
// or unparseable. Used as a positional-arg fallback by tab-targeting
// commands.
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

// debugEnabled reports whether stderr debug logging should be emitted.
// True when ROOST_DEBUG is set OR -v/--verbose was passed at least once.
func debugEnabled() bool {
	return os.Getenv("ROOST_DEBUG") != "" || flagVerbose > 0
}
