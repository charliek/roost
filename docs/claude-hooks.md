# Wiring Claude Code into Roost

Roost surfaces a notification whenever an agent in a tab needs your attention. The cleanest way to drive this from Claude Code is its hook system: when Claude finishes a turn (`Stop`) or asks for permission (`Notification`), have it call `roost-cli notify`.

## Prerequisites

You need both binaries on `$PATH`:

```bash
make
sudo cp roost-cli /usr/local/bin/    # or symlink, or whatever fits your setup
```

The Roost GUI must be running. When Roost spawns each tab's shell, it injects two environment variables that `roost-cli` reads:

- `ROOST_TAB_ID` — the integer tab ID this shell belongs to
- `ROOST_SOCKET` — the path to Roost's Unix socket

So `roost-cli` invoked from inside any Roost tab knows which tab to notify automatically.

## Settings

Add to `~/.claude/settings.json` (or your project-local `.claude/settings.json` for per-repo behavior):

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

The hook command runs in the same shell environment as the agent, so `ROOST_TAB_ID` and `ROOST_SOCKET` are inherited automatically. No extra wiring needed.

## Verifying

Open a tab in Roost. Run:

```bash
roost-cli identify
```

Should print JSON describing the running app, the socket path, and the active tab. If it errors out, the GUI isn't running or the env vars aren't set.

Now run a notification by hand:

```bash
roost-cli notify --title "test" --body "hello from the cli"
```

Switch to a *different* Roost tab, then re-run the command. You should see:

- The originating tab's tab strip entry pick up the libadwaita "needs attention" indicator.
- A native desktop notification with the title and body.

## Forcing a different tab

If you want a hook fired in tab A to notify tab B (rare), pass `--tab`:

```bash
roost-cli notify --tab 7 --title "..." --body "..."
```

Tab IDs are visible in `roost-cli identify` output and in the SQLite database at `~/Library/Application Support/Roost/roost.db` on macOS.

## OSC fallback

If you're using a tool that doesn't speak the Roost CLI, Roost also recognizes two OSC notification formats from any process running in a tab:

```bash
# iTerm2 / general
printf '\033]9;Build done\007'

# Konsole / KDE
printf '\033]777;notify;Title;Body text\007'
```

These produce the same UI effect as `roost-cli notify`. The CLI path is preferred since it carries structured fields and bypasses VT parsing.
