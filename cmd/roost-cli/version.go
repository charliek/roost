package main

import (
	"fmt"

	"github.com/spf13/cobra"
)

// version is the printed version string. Build-time stamping (ldflags
// or an internal/version package, like prox/shed) is deliberately
// deferred — keep this a single editable constant for now.
var version = "dev"

var versionCmd = &cobra.Command{
	Use:   "version",
	Short: "Print the roost-cli version",
	Args:  cobra.NoArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		if clientCtx.JSON {
			return outputJSON(struct {
				Version string `json:"version"`
			}{Version: version})
		}
		fmt.Printf("roost-cli %s\n", version)
		return nil
	},
}

func init() {
	rootCmd.AddCommand(versionCmd)
}
