package main

import (
	"os"

	"github.com/spf13/cobra"
)

var completionCmd = &cobra.Command{
	Use:   "completion [bash|zsh|fish|powershell]",
	Short: "Generate shell completion scripts",
	Long: `Generate a shell completion script for roost-cli.

To load completions:

Bash:
  source <(roost-cli completion bash)
  # macOS, persistent:
  roost-cli completion bash > $(brew --prefix)/etc/bash_completion.d/roost-cli
  # Linux, persistent:
  roost-cli completion bash > /etc/bash_completion.d/roost-cli

Zsh:
  # If completion isn't already enabled in your shell:
  echo "autoload -U compinit; compinit" >> ~/.zshrc

  roost-cli completion zsh > "${fpath[1]}/_roost-cli"
  # Restart your shell.

Fish:
  roost-cli completion fish | source
  # Persistent:
  roost-cli completion fish > ~/.config/fish/completions/roost-cli.fish

PowerShell:
  roost-cli completion powershell | Out-String | Invoke-Expression
`,
	DisableFlagsInUseLine: true,
	ValidArgs:             []string{"bash", "zsh", "fish", "powershell"},
	Args:                  cobra.MatchAll(cobra.ExactArgs(1), cobra.OnlyValidArgs),
	RunE: func(cmd *cobra.Command, args []string) error {
		switch args[0] {
		case "bash":
			return cmd.Root().GenBashCompletion(os.Stdout)
		case "zsh":
			return cmd.Root().GenZshCompletion(os.Stdout)
		case "fish":
			return cmd.Root().GenFishCompletion(os.Stdout, true)
		case "powershell":
			return cmd.Root().GenPowerShellCompletionWithDesc(os.Stdout)
		}
		return nil
	},
}

func init() {
	rootCmd.AddCommand(completionCmd)
}
