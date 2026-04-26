package main

import (
	"fmt"
	"os"
)

// Companion CLI used from inside a Roost tab to fire notifications,
// set the title, etc. Talks to the running roost app over a Unix socket.
// Implemented in Phase 3.
func main() {
	fmt.Fprintln(os.Stderr, "roost-cli: not yet implemented (phase 3)")
	os.Exit(1)
}
