# Claude Code Hooks

Wire Claude Code's hook system to `roost-cli notify` so you get a notification whenever an agent finishes a turn or asks for permission.

## Prerequisites

- `roost` (GUI) running.
- `roost-cli` on `PATH`. See [Installation](../getting-started/installation.md#macos-homebrew) for the `install -m 755` step.
- Roost's spawned shell is your Claude Code execution environment. The hook command runs inside that shell, so it inherits `ROOST_TAB_ID` and `ROOST_SOCKET` automatically.

## Settings

Add to `~/.claude/settings.json` (global) or `.claude/settings.json` in a repository (per-project):

```json
{
  "hooks": {
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "roost-cli notify --title \"Claude is done\" --body \"turn complete\""
          }
        ]
      }
    ],
    "Notification": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "roost-cli notify --title \"Claude needs you\" --body \"awaiting input\""
          }
        ]
      }
    ]
  }
}
```

Both hooks run in the agent's working shell. No env wiring required.

## Verifying

Open a fresh Roost tab, then:

```bash
roost-cli identify
```

You should see a JSON object describing the running app. If it errors, the GUI isn't running or `ROOST_SOCKET` is unset — re-launch `roost` and try again.

Now fire a manual notification:

```bash
roost-cli notify --title "test" --body "from the cli"
```

Switch to a different Roost tab and run it again. The originating tab should pick up the `needs attention` indicator and you should see a desktop notification.

## Targeting a different tab

The tab id is resolved from `--tab` first, then `$ROOST_TAB_ID`. To send to a tab from outside its shell:

```bash
roost-cli notify --tab 7 --title "..." --body "..."
```

Tab ids are visible from `roost-cli identify` and in the SQLite database (see [Paths & Environment](../reference/paths.md)).

## Why CLI and not OSC?

The CLI path carries structured fields, doesn't depend on the terminal parser, and survives stdout buffering. OSC 9 / OSC 777 are supported as fallbacks for tools that can't be modified — see [Notifications](notifications.md) for that path.
