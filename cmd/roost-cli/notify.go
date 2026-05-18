package main

import (
	"fmt"

	"github.com/spf13/cobra"
)

var notifyTabFlag int64

var notifyCmd = &cobra.Command{
	Use:   "notify TITLE [BODY]",
	Short: "Send a desktop notification through the Roost GUI",
	Long: `Send a desktop notification routed by the running Roost GUI.

The notification is associated with a tab; clicking it focuses that tab.
TITLE is required and positional. BODY is optional.

Tab id falls back to $ROOST_TAB_ID when --tab is not given. Pass --
to disambiguate titles starting with a dash:

  roost-cli notify -- "-leading-dash-is-fine"

Examples:
  roost-cli notify "Build done"
  roost-cli notify "Build done" "All tests passed"
  roost-cli notify "Heads up" --tab 7`,
	Args: cobra.RangeArgs(1, 2),
	RunE: runNotify,
}

func runNotify(cmd *cobra.Command, args []string) error {
	title := args[0]
	body := ""
	if len(args) == 2 {
		body = args[1]
	}
	tabID := notifyTabFlag
	if tabID == 0 {
		tabID = tabIDFromEnv()
	}

	if err := newClient().Notify(tabID, title, body); err != nil {
		return err
	}
	if clientCtx.JSON {
		return outputJSON(ActionResult{
			Status: "ok",
			Action: "notify",
			Name:   fmt.Sprintf("%d", tabID),
		})
	}
	if tabID == 0 {
		printSuccess("Notification sent")
	} else {
		printSuccess("Notification sent to tab %d", tabID)
	}
	return nil
}

func init() {
	notifyCmd.Flags().Int64Var(&notifyTabFlag, "tab", 0, "Tab id (defaults to $ROOST_TAB_ID)")
	rootCmd.AddCommand(notifyCmd)
}
