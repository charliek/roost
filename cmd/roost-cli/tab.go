package main

import (
	"fmt"
	"strconv"

	"github.com/charliek/roost/internal/ipc"
	"github.com/spf13/cobra"
)

var tabCmd = &cobra.Command{
	Use:   "tab",
	Short: "Manage Roost tabs",
	Long:  `Operate on tabs of the running Roost GUI: list, focus, retitle, set agent state.`,
}

// --- tab list -------------------------------------------------------

var tabListCmd = &cobra.Command{
	Use:   "list",
	Short: "List projects and tabs in the running GUI",
	Long: `List all projects and their tabs from the running Roost GUI.

Default output is a tree grouped by project. --json emits the typed
TabListResult payload (suitable for scripts and agents).

Examples:
  roost-cli tab list
  roost-cli --json tab list`,
	Args: cobra.NoArgs,
	RunE: runTabList,
}

func runTabList(cmd *cobra.Command, args []string) error {
	tree, err := newClient().TabList()
	if err != nil {
		return err
	}
	if clientCtx.JSON {
		return outputJSON(tree)
	}
	printTabTree(tree)
	return nil
}

// printTabTree renders a TabListResult for humans. Project-grouped,
// active tabs marked with `*`, notification badges with `[!]`, and the
// agent state inline.
func printTabTree(tree ipc.TabListResult) {
	for _, p := range tree.Projects {
		fmt.Printf("%s (id=%d)\n", p.Name, p.ID)
		for _, t := range p.Tabs {
			marker := "  "
			if t.IsActive {
				marker = "* "
			}
			notif := ""
			if t.HasNotification {
				notif = " [!]"
			}
			state := t.AgentState
			if state == "" || state == "none" {
				state = "-"
			}
			fmt.Printf("%s[%d] %s  state=%s%s\n", marker, t.ID, t.Title, state, notif)
		}
	}
}

// --- tab focus ------------------------------------------------------

var tabFocusCmd = &cobra.Command{
	Use:   "focus [TAB_ID]",
	Short: "Switch the GUI to a tab and raise the window",
	Long: `Focus a tab by id. Switches the active project (if needed),
selects the tab, raises the window, and grabs focus on the terminal.

TAB_ID is positional; falls back to $ROOST_TAB_ID when omitted.

Examples:
  roost-cli tab focus 7
  ROOST_TAB_ID=7 roost-cli tab focus`,
	Args:              cobra.MaximumNArgs(1),
	RunE:              runTabFocus,
	ValidArgsFunction: completeTabIDs,
}

func runTabFocus(cmd *cobra.Command, args []string) error {
	var tabID int64
	if len(args) == 1 {
		v, err := strconv.ParseInt(args[0], 10, 64)
		if err != nil {
			return errUsageMsg("tab focus: invalid TAB_ID %q", args[0])
		}
		tabID = v
	} else {
		tabID = tabIDFromEnv()
	}
	if tabID == 0 {
		return errUsageMsg("tab focus: TAB_ID required (positional or $ROOST_TAB_ID)")
	}

	prev, err := newClient().TabFocus(tabID)
	if err != nil {
		return err
	}
	if clientCtx.JSON {
		return outputJSON(ActionResult{
			Status:  "ok",
			Action:  "focus",
			Name:    fmt.Sprintf("%d", tabID),
			Details: prev,
		})
	}
	printSuccess("Focused tab %d", tabID)
	return nil
}

// --- tab set-title --------------------------------------------------

var tabSetTitleFlag int64

var tabSetTitleCmd = &cobra.Command{
	Use:   "set-title TITLE",
	Short: "Rename a tab",
	Long: `Rename a tab. TITLE is required and positional.

Tab id falls back to $ROOST_TAB_ID when --tab is not given.

Examples:
  roost-cli tab set-title "build watcher"
  roost-cli tab set-title "build watcher" --tab 7`,
	Args: cobra.ExactArgs(1),
	RunE: runTabSetTitle,
}

func runTabSetTitle(cmd *cobra.Command, args []string) error {
	title := args[0]
	tabID := tabSetTitleFlag
	if tabID == 0 {
		tabID = tabIDFromEnv()
	}
	if tabID == 0 {
		return errUsageMsg("tab set-title: --tab required (or set $ROOST_TAB_ID)")
	}

	if err := newClient().TabSetTitle(tabID, title); err != nil {
		return err
	}
	if clientCtx.JSON {
		return outputJSON(ActionResult{
			Status: "ok", Action: "set-title", Name: fmt.Sprintf("%d", tabID),
		})
	}
	printSuccess("Tab %d titled %q", tabID, title)
	return nil
}

// --- tab set-state --------------------------------------------------

var tabSetStateFlag int64

var tabSetStateCmd = &cobra.Command{
	Use:   "set-state STATE",
	Short: "Set the sticky agent state for a tab",
	Long: `Set the per-tab agent state. STATE is one of:
none|running|needs_input|idle.

Tab id falls back to $ROOST_TAB_ID when --tab is not given.

Examples:
  roost-cli tab set-state running --tab 7
  ROOST_TAB_ID=7 roost-cli tab set-state idle`,
	Args:      cobra.ExactArgs(1),
	ValidArgs: []string{"none", "running", "needs_input", "idle"},
	RunE:      runTabSetState,
}

func runTabSetState(cmd *cobra.Command, args []string) error {
	state := args[0]
	tabID := tabSetStateFlag
	if tabID == 0 {
		tabID = tabIDFromEnv()
	}
	if tabID == 0 {
		return errUsageMsg("tab set-state: --tab required (or set $ROOST_TAB_ID)")
	}

	if err := newClient().TabSetState(tabID, state); err != nil {
		return err
	}
	if clientCtx.JSON {
		return outputJSON(ActionResult{
			Status: "ok", Action: "set-state", Name: fmt.Sprintf("%d", tabID),
			Details: map[string]string{"state": state},
		})
	}
	printSuccess("Tab %d state=%s", tabID, state)
	return nil
}

// --- completion -----------------------------------------------------

// completeTabIDs powers shell completion for tab id arguments. Returns
// "id\ttitle" entries so zsh/fish display the title alongside the id.
// Silently returns nothing if the GUI isn't reachable — completion
// must not error or block.
func completeTabIDs(cmd *cobra.Command, args []string, toComplete string) ([]string, cobra.ShellCompDirective) {
	if len(args) > 0 {
		return nil, cobra.ShellCompDirectiveNoFileComp
	}
	tree, err := newClient().TabList()
	if err != nil {
		return nil, cobra.ShellCompDirectiveNoFileComp
	}
	var out []string
	for _, p := range tree.Projects {
		for _, t := range p.Tabs {
			out = append(out, fmt.Sprintf("%d\t%s/%s", t.ID, p.Name, t.Title))
		}
	}
	return out, cobra.ShellCompDirectiveNoFileComp
}

func init() {
	tabSetTitleCmd.Flags().Int64Var(&tabSetTitleFlag, "tab", 0, "Tab id (defaults to $ROOST_TAB_ID)")
	tabSetStateCmd.Flags().Int64Var(&tabSetStateFlag, "tab", 0, "Tab id (defaults to $ROOST_TAB_ID)")

	// Flag-completion for --tab on commands where the tab id is the
	// flag value rather than a positional. ValidArgsFunction would
	// not apply here because the positional is TITLE/STATE.
	_ = tabSetTitleCmd.RegisterFlagCompletionFunc("tab", completeTabIDs)
	_ = tabSetStateCmd.RegisterFlagCompletionFunc("tab", completeTabIDs)

	tabCmd.AddCommand(tabListCmd, tabFocusCmd, tabSetTitleCmd, tabSetStateCmd)
	rootCmd.AddCommand(tabCmd)
}
