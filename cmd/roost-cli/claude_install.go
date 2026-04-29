package main

import (
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"os"
	"path/filepath"

	"github.com/charliek/roost/internal/config"
)

// cmdClaude dispatches `roost-cli claude <subcommand>`. Today only
// `install` exists; uninstall is a one-liner the user can do (delete
// the file + remove the alias) and isn't worth its own command.
func cmdClaude(args []string) int {
	if len(args) == 0 {
		fmt.Fprintln(os.Stderr, "roost claude: subcommand required (install)")
		return 2
	}
	switch args[0] {
	case "install":
		return cmdClaudeInstall(args[1:])
	default:
		fmt.Fprintf(os.Stderr, "roost claude: unknown subcommand %q\n", args[0])
		return 2
	}
}

// cmdClaudeInstall writes ~/.config/roost/claude-settings.json with
// hooks pointing at the absolute path of the running roost-cli binary,
// then prints a bash alias snippet to stdout. The user pastes the
// snippet into their shell rc.
//
// The settings file is loaded by Claude via `claude --settings <path>`
// — Claude merges it into the user's other settings sources, so we
// only own the hook entries.
func cmdClaudeInstall(args []string) int {
	fs := flag.NewFlagSet("claude install", flag.ContinueOnError)
	force := fs.Bool("force", false, "overwrite an existing claude-settings.json")
	if err := fs.Parse(args); err != nil {
		return 2
	}

	paths, err := config.Resolve()
	if err != nil {
		fmt.Fprintf(os.Stderr, "roost claude install: resolve paths: %v\n", err)
		return 1
	}
	if err := paths.EnsureDirs(); err != nil {
		fmt.Fprintf(os.Stderr, "roost claude install: mkdir: %v\n", err)
		return 1
	}

	settingsPath := paths.ClaudeSettingsPath()
	if !*force {
		if _, err := os.Stat(settingsPath); err == nil {
			fmt.Fprintf(os.Stderr, "roost claude install: %s already exists; use --force to overwrite\n", settingsPath)
			return 1
		} else if !errors.Is(err, os.ErrNotExist) {
			fmt.Fprintf(os.Stderr, "roost claude install: stat: %v\n", err)
			return 1
		}
	}

	cliPath, err := absoluteCLIPath()
	if err != nil {
		fmt.Fprintf(os.Stderr, "roost claude install: locate roost-cli: %v\n", err)
		return 1
	}

	doc := buildClaudeSettings(cliPath)
	enc, err := json.MarshalIndent(doc, "", "  ")
	if err != nil {
		fmt.Fprintf(os.Stderr, "roost claude install: marshal: %v\n", err)
		return 1
	}
	enc = append(enc, '\n')
	if err := os.WriteFile(settingsPath, enc, 0o600); err != nil {
		fmt.Fprintf(os.Stderr, "roost claude install: write %s: %v\n", settingsPath, err)
		return 1
	}

	fmt.Fprintf(os.Stderr, "# Wrote %s\n", settingsPath)
	fmt.Fprintln(os.Stderr, "# Add the line below to your shell rc (e.g. ~/.bashrc), then 'source ~/.bashrc'.")
	fmt.Fprintln(os.Stderr, "# Fish/zsh: adapt the alias syntax for your shell.")
	fmt.Println()
	fmt.Println("# Roost: route Claude Code hooks to the running GUI.")
	fmt.Printf("alias claude='claude --settings %s'\n", settingsPath)
	return 0
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
						"command": fmt.Sprintf("%s claude-hook %s", quoteForShell(cliPath), event),
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

// absoluteCLIPath returns the absolute path of the running roost-cli
// binary. Used to populate the hook command lines so they survive
// across shell PATH changes.
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
// characters that bash would interpret. Single-quote escaping handles
// embedded single quotes too. Used for the alias and the hook command
// strings, both of which are shell-parsed.
func quoteForShell(s string) string {
	for _, c := range s {
		if c == ' ' || c == '\t' || c == '"' || c == '$' || c == '\\' || c == '`' {
			return "'" + escapeSingleQuote(s) + "'"
		}
	}
	return s
}

// escapeSingleQuote escapes embedded single quotes for bash's
// '...'\''...' pattern.
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
