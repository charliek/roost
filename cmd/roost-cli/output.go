package main

import (
	"encoding/json"
	"fmt"
	"os"
)

// ActionResult is the standard JSON envelope for mutating commands
// (notify, tab focus, tab set-title, tab set-state). Read commands
// emit their own typed payloads.
type ActionResult struct {
	Status  string `json:"status"`
	Action  string `json:"action"`
	Name    string `json:"name,omitempty"`
	Details any    `json:"details,omitempty"`
}

// outputJSON writes v as indented JSON to os.Stdout. Always uses the
// real os.Stdout — never cobra's command writer — so the hook's
// strict invariants hold even if a future command calls this.
func outputJSON(v any) error {
	enc := json.NewEncoder(os.Stdout)
	enc.SetIndent("", "  ")
	return enc.Encode(v)
}

// outputError writes a JSON {"error": "..."} object to os.Stderr.
// Used by main() in JSON mode so callers parsing stdout don't trip
// over error envelopes mixed into their data stream.
func outputError(err error) error {
	enc := json.NewEncoder(os.Stderr)
	enc.SetIndent("", "  ")
	return enc.Encode(struct {
		Error string `json:"error"`
	}{Error: err.Error()})
}

// printSuccess writes a checkmarked one-liner to stdout in human mode.
// No-op in JSON mode — callers should emit an ActionResult instead.
func printSuccess(format string, args ...any) {
	if clientCtx.JSON {
		return
	}
	fmt.Printf("✓ "+format+"\n", args...)
}

// printError writes a message + optional suggestions to stderr in
// human mode. No-op in JSON mode — main() handles error rendering
// via outputError instead.
func printError(msg string, suggestions ...string) {
	if clientCtx.JSON {
		return
	}
	fmt.Fprintf(os.Stderr, "Error: %s\n", msg)
	if len(suggestions) > 0 {
		fmt.Fprintln(os.Stderr, "\nTry:")
		for _, s := range suggestions {
			fmt.Fprintf(os.Stderr, "  %s\n", s)
		}
	}
}
