package main

import (
	"fmt"

	"github.com/spf13/cobra"
)

var identifyCmd = &cobra.Command{
	Use:   "identify",
	Short: "Print details about the running Roost GUI",
	Long: `Identify connects to the GUI and prints its socket path, PID, and
the currently active project/tab.

Default output is a human-readable key-value list; --json emits the
typed Identity payload.

Examples:
  roost-cli identify
  roost-cli --json identify`,
	Args: cobra.NoArgs,
	RunE: runIdentify,
}

func runIdentify(cmd *cobra.Command, args []string) error {
	id, err := newClient().Identify()
	if err != nil {
		return err
	}
	if clientCtx.JSON {
		return outputJSON(id)
	}
	fmt.Printf("socket:            %s\n", id.SocketPath)
	fmt.Printf("pid:               %d\n", id.PID)
	fmt.Printf("active_project_id: %d\n", id.ActiveProjectID)
	fmt.Printf("active_tab_id:     %d\n", id.ActiveTabID)
	return nil
}

func init() {
	rootCmd.AddCommand(identifyCmd)
}
