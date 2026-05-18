package main

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"

	"github.com/charliek/roost/internal/config"
	"github.com/spf13/cobra"
)

var claudeCmd = &cobra.Command{
	Use:   "claude",
	Short: "Claude Code integration helpers",
	Long: `Subcommands for wiring Claude Code to the running Roost GUI.

  install   Generate a settings file pointing at this binary.
  hook      Hook handler invoked by Claude (used by 'install').`,
}

// --- claude install -------------------------------------------------

var claudeInstallForce bool

var claudeInstallCmd = &cobra.Command{
	Use:   "install",
	Short: "Generate a Claude Code settings file pointing at this binary",
	Long: `Write ~/.config/roost/claude-settings.json with hooks pointing at
the absolute path of the running roost-cli binary, then print a shell
alias snippet to stdout.

The alias goes to STDOUT (so 'roost-cli claude install >> ~/.bashrc'
appends just the alias). The "Wrote ..." status message goes to
STDERR.

Examples:
  roost-cli claude install
  roost-cli claude install --force
  roost-cli claude install >> ~/.bashrc`,
	Args: cobra.NoArgs,
	RunE: runClaudeInstall,
}

func runClaudeInstall(cmd *cobra.Command, args []string) error {
	// JSON output is meaningless for this command — its product is a
	// shell snippet, not data. Reject explicitly so scripts don't get
	// silently broken output.
	if clientCtx.JSON {
		return errUsageMsg("claude install: --json not supported (output is a shell snippet)")
	}

	paths, err := config.Resolve()
	if err != nil {
		return fmt.Errorf("resolve paths: %w", err)
	}
	if err := paths.EnsureDirs(); err != nil {
		return fmt.Errorf("mkdir: %w", err)
	}

	settingsPath := paths.ClaudeSettingsPath()
	if !claudeInstallForce {
		if _, err := os.Stat(settingsPath); err == nil {
			return fmt.Errorf("%s already exists; use --force to overwrite", settingsPath)
		} else if !errors.Is(err, os.ErrNotExist) {
			return fmt.Errorf("stat: %w", err)
		}
	}

	cliPath, err := absoluteCLIPath()
	if err != nil {
		return fmt.Errorf("locate roost-cli: %w", err)
	}

	doc := buildClaudeSettings(cliPath)
	enc, err := json.MarshalIndent(doc, "", "  ")
	if err != nil {
		return fmt.Errorf("marshal: %w", err)
	}
	enc = append(enc, '\n')
	if err := os.WriteFile(settingsPath, enc, 0o600); err != nil {
		return fmt.Errorf("write %s: %w", settingsPath, err)
	}

	// CRITICAL stdout/stderr split:
	//   - alias snippet → stdout (so '>> ~/.bashrc' appends just it)
	//   - status messages → stderr
	// Do NOT replace these with printSuccess (which writes to stdout)
	// or fmt.Println (which goes to stdout). The split is observable
	// behavior tested in claude_install_test.go.
	fmt.Fprintf(os.Stderr, "# Wrote %s\n", settingsPath)
	fmt.Fprintln(os.Stderr, "# Add the line below to your shell rc (e.g. ~/.bashrc), then 'source ~/.bashrc'.")
	fmt.Fprintln(os.Stderr, "# Fish/zsh: adapt the alias syntax for your shell.")
	fmt.Println()
	fmt.Println("# Roost: route Claude Code hooks to the running GUI.")
	fmt.Printf("alias claude='claude --settings '%s\n", quoteForShell(settingsPath))
	return nil
}

// buildClaudeSettings produces the settings.json shape Claude expects:
// hooks keyed by event name, each entry a list of one or more matchers
// containing a list of command hooks.
func buildClaudeSettings(cliPath string) map[string]any {
	hookFor := func(event string) any {
		return []any{
			map[string]any{
				"hooks": []any{
					map[string]any{
						"type":    "command",
						"command": fmt.Sprintf("%s claude hook %s", quoteForShell(cliPath), event),
					},
				},
			},
		}
	}
	return map[string]any{
		"hooks": map[string]any{
			"SessionStart":     hookFor("session-start"),
			"UserPromptSubmit": hookFor("prompt-submit"),
			"Notification":     hookFor("notification"),
			"Stop":             hookFor("stop"),
			"SessionEnd":       hookFor("session-end"),
		},
	}
}

// absoluteCLIPath returns the absolute, symlink-resolved path of the
// running binary. Used in the generated settings so hooks survive
// PATH changes.
func absoluteCLIPath() (string, error) {
	exe, err := os.Executable()
	if err != nil {
		return "", err
	}
	abs, err := filepath.Abs(exe)
	if err != nil {
		return "", err
	}
	return filepath.EvalSymlinks(abs)
}

// quoteForShell wraps a path in single quotes if it contains
// shell-meaningful characters. Used for both the alias body and the
// generated hook command lines.
func quoteForShell(s string) string {
	for _, c := range s {
		if c == ' ' || c == '\t' || c == '"' || c == '$' || c == '\\' || c == '`' || c == '\'' {
			return "'" + escapeSingleQuote(s) + "'"
		}
	}
	return s
}

// escapeSingleQuote escapes embedded single quotes using bash's
// close-quote/escape/open-quote pattern: ' becomes '\''.
func escapeSingleQuote(s string) string {
	out := make([]byte, 0, len(s))
	for i := 0; i < len(s); i++ {
		if s[i] == '\'' {
			out = append(out, []byte("'\\''")...)
		} else {
			out = append(out, s[i])
		}
	}
	return string(out)
}

func init() {
	claudeInstallCmd.Flags().BoolVar(&claudeInstallForce, "force", false, "Overwrite an existing claude-settings.json")
	claudeCmd.AddCommand(claudeInstallCmd, claudeHookCmd)
	rootCmd.AddCommand(claudeCmd)
}
