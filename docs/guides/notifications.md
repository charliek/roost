# Notifications

Roost's notification pipeline has three input paths and two output surfaces. All input paths converge on the same internal event, so the user-visible behavior is identical no matter how the notification was triggered.

## Input paths

| Source                    | Triggered by                                             | Best for                                            |
|---------------------------|----------------------------------------------------------|-----------------------------------------------------|
| `roost-cli notify`        | A process running inside a Roost tab                     | Claude Code hooks, build scripts, structured pings  |
| OSC 9 escape sequence     | Any process printing `\x1b]9;<message>\x07`              | iTerm2-style apps that already emit OSC 9           |
| OSC 777 escape sequence   | Any process printing `\x1b]777;notify;<title>;<body>\x07`| Konsole / KDE-style apps                            |

`roost-cli` is the preferred path because it carries structured fields (separate title and body, target tab) and bypasses VT parsing. The OSC paths exist as a fallback for tools that can't be modified.

## Output surfaces

When a notification arrives for a tab that is not currently focused:

1. The tab's `needs attention` indicator turns on (a subtle blue underline / dot, depending on libadwaita version).
2. A native desktop notification fires:
   - macOS: via `osascript` → Notification Center
   - Linux: via `gio.Notification` → freedesktop notification daemon

If the target tab *is* the currently focused one, both surfaces are suppressed — you are already looking at it.

The tab indicator clears when you select that tab.

## Tab targeting

`roost-cli` resolves the target tab in this order:

1. The `--tab <id>` flag, if provided
2. The `ROOST_TAB_ID` environment variable, set by Roost when it spawns each tab's shell
3. Error: tab id required

Roost injects two environment variables into every spawned shell:

| Variable        | Value                                                              |
|-----------------|--------------------------------------------------------------------|
| `ROOST_TAB_ID`  | The integer tab id this shell is bound to                          |
| `ROOST_SOCKET`  | The Unix-socket path the GUI is listening on                       |

So `roost-cli` invoked from inside any tab needs no flags or config — it knows where to send and which tab to mark.

## Examples

From inside a Roost tab:

```bash
roost-cli notify --title "Build done" --body "tests pass"
```

From outside Roost, target a specific tab:

```bash
ROOST_SOCKET="$HOME/Library/Application Support/Roost/roost.sock" \
  roost-cli notify --title "From CI" --body "deploy ready" --tab 3
```

OSC 9 from any shell inside a Roost tab:

```bash
printf '\033]9;Build done\007'
```

OSC 777 with a separate title and body:

```bash
printf '\033]777;notify;Title;Body text\007'
```

## What you do not have to do

- You do not have to configure the socket path manually (`ROOST_SOCKET` is auto-set).
- You do not have to track tab ids manually (`ROOST_TAB_ID` is auto-set).
- You do not have to add anything to your shell config — the env vars come from the parent process.

## Limits and caveats

- Body length is capped at 8 KB on the OSC parser to bound buffer growth on a misbehaving sender. Longer bodies are truncated.
- A second notification on the same tab supersedes the first in the desktop notification stream — Roost uses a per-tab notification id.
- macOS Notification Center may need permission for `osascript` (or for `roost` itself) the first time. Allow it in **System Settings → Notifications**.

See the [Claude Code Hooks](claude-code.md) guide for the most common Claude Code wire-up.
