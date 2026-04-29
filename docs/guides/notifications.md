# Notifications

Roost's notification pipeline has three input paths and three output surfaces. All input paths converge on the same internal events, so the user-visible behavior is identical no matter how the notification was triggered.

## Input paths

| Source                    | Triggered by                                             | Best for                                            |
|---------------------------|----------------------------------------------------------|-----------------------------------------------------|
| `roost-cli notify`        | A process running inside a Roost tab                     | Claude Code hooks, build scripts, structured pings  |
| OSC 9 escape sequence     | Any process printing `\x1b]9;<message>\x07`              | iTerm2-style apps that already emit OSC 9           |
| OSC 777 escape sequence   | Any process printing `\x1b]777;notify;<title>;<body>\x07`| Konsole / KDE-style apps                            |

`roost-cli` is the preferred path because it carries structured fields (separate title and body, target tab) and bypasses VT parsing. The OSC paths exist as a fallback for tools that can't be modified.

OSC 9 disambiguation: bodies starting with a digit followed by `;` (or only digits) are treated as ConEmu extensions (sleep / progress / message-box / etc.) and dropped — they are not iTerm2-style notifications. This is why Claude Code's frequent OSC 9;4 progress pings don't surface as banners. A genuine iTerm2 notification whose text happens to start with a digit (`\x1b]9;1 file changed\x07`) still passes through unchanged.

## Output surfaces

A notification has three places it can show up. They are independent — clearing one does not clear the others, except where noted.

1. **Pending-attention badge on the tab.** The built-in libadwaita "needs attention" pulse (a subtle dot / underline). Set when a notification arrives for a non-focused tab; cleared when you select that tab.
2. **Sticky agent-state indicator on the tab.** A small colored circle next to the title — blue (running), orange (needs your input), gray (idle / turn complete), or none. State only changes from agent hook events (`roost-cli claude-hook ...` or future equivalents); it survives focus events.
3. **Project rollup stripe on the sidebar row.** A 3px left-edge color stripe colored by the highest-severity state across the project's tabs. **needs-input wins** because the most actionable signal should dominate — a project with one blocked tab and four running tabs flags the user, not "busy."
4. **Desktop notification banner.**
   - macOS: shells out to `terminal-notifier` (Homebrew). Click the banner → `roost-cli tab focus --tab N` runs → window raises and the right tab becomes active. Without `terminal-notifier` installed, banners are silent no-ops; in-app indicators still work. (Distribution will declare it as a Homebrew dependency.)
   - Linux: `gio.Notification` → freedesktop notification daemon over DBus, with a default action wired to the in-process `app.tab-focus` GIO action. Clicking the banner focuses the tab in-process — no IPC round-trip needed.

If the target tab *is* the currently focused one and the window is active, the badge and the desktop banner are both suppressed — you're already looking at it. State indicators still update.

## Per-tab cooldown

Identical `(title, body)` pairs on the same tab inside one second are dropped silently. Distinct content fires immediately. This protects against scripts that double-fire and against pathological OSC streams that slip past the ConEmu filter.

## Tab targeting

`roost-cli` resolves the target tab in this order:

1. The `--tab <id>` flag, if provided
2. The `ROOST_TAB_ID` environment variable, set by Roost when it spawns each tab's shell
3. Error: tab id required

Roost injects three environment variables into every spawned shell:

| Variable           | Value                                                              |
|--------------------|--------------------------------------------------------------------|
| `ROOST_TAB_ID`     | The integer tab id this shell is bound to                          |
| `ROOST_PROJECT_ID` | The integer project id this tab lives in                           |
| `ROOST_SOCKET`     | The Unix-socket path the GUI is listening on                       |

So `roost-cli` invoked from inside any tab needs no flags or config — it knows where to send and which tab to mark.

## Hook-session OSC suppression

When a structured hook session is driving a tab (e.g. Claude Code with `roost-cli claude install` wired up), raw OSC 9 / 777 from inside the agent is dropped. Hooks are the trusted channel; OSC is the fallback for tools that can't be modified, and hook-driven agents emit OSC noise we don't want to surface twice. The suppression is automatic — `session-start` engages it, `session-end` releases it.

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

Manually drive the agent-state indicator (e.g. for a non-Claude agent):

```bash
roost-cli tab set-state --tab 3 --state needs_input
roost-cli tab set-state --tab 3 --state idle
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
- A second desktop notification on the same tab supersedes the first via `terminal-notifier -group` on macOS and the GApplication notification id on Linux. Supersede on macOS Sonoma+ is best-effort for unsigned senders.
- On Wayland, `gtk.Window.Present()` without an XDG-activation token may only flash the taskbar instead of raising. Click-from-banner paths typically pass a token through; CLI scripts that call `roost-cli tab focus` directly may not.
- macOS banners are currently branded as "terminal-notifier" rather than "Roost." Roost-branded banners need a code-signed `.app` bundle (Layer 3, separate work).

See the [Claude Code Hooks](claude-code.md) guide for the most common Claude Code wire-up.
