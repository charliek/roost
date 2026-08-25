# Notifications

Roost's notification pipeline has three input paths and four output surfaces. All input paths converge on the same internal events, so the user-visible behavior is identical no matter how the notification was triggered.

## Input paths

| Source                    | Triggered by                                             | Best for                                            |
|---------------------------|----------------------------------------------------------|------------------------------------------------------|
| `roostctl notify`        | A process running inside a Roost tab                     | Claude Code hooks, build scripts, structured pings  |
| OSC 9 escape sequence     | Any process printing `\x1b]9;<message>\x07`              | iTerm2-style apps that already emit OSC 9           |
| OSC 777 escape sequence   | Any process printing `\x1b]777;notify;<title>;<body>\x07`| Konsole / KDE-style apps                            |

`roostctl` is the preferred path because it carries structured fields (separate title and body, target tab) and bypasses VT parsing. The OSC paths exist as a fallback for tools that can't be modified.

OSC 9 disambiguation: bodies starting with a digit followed by `;` (or only digits) are treated as ConEmu extensions (sleep / progress / message-box / etc.) and dropped — they are not iTerm2-style notifications. This is why Claude Code's frequent OSC 9;4 progress pings don't surface as banners. A genuine iTerm2 notification whose text happens to start with a digit (`\x1b]9;1 file changed\x07`) still passes through unchanged.

## Output surfaces

A notification has four places it can show up. Under normal use three of them move together — see **Focus policy** below for why — while the sticky indicator is the deliberate exception.

1. **Pending-attention badge on the tab.** The built-in libadwaita "needs attention" pulse (a subtle dot / underline). Set when a notification is delivered for a non-focused tab; cleared when you select that tab.
2. **Sticky agent-state indicator on the tab.** A small colored circle next to the title: blue (running), orange (needs your input), gray (idle / turn complete), red (agent failed), or none. This is the tab's *effective* state — the shell's own OSC 133 marks (a foreground process reads blue) when no agent owns the tab, or the owning agent's own lifecycle (`roostctl claude-hook ...`, `roostctl tab set-state`) when one does. It is never focus-cleared; see **Focus policy**.
3. **Project rollup stripe on the sidebar row.** A 3px left-edge color stripe colored by the highest-ranked state across the project's tabs — ranked `failed > needs-input > running > idle`, so a dead/errored agent's own color outranks a merely-blocked one. A tab a hook owns participates in this ranking like any other tab; it is not excluded (a project whose only blocked tab is a Claude session gets a stripe, not silence).
4. **Desktop notification banner.**
   - macOS: `UNUserNotificationCenter` (`mac/Sources/Roost/DesktopNotifications.swift`). Click the banner and the tab is focused in-process — no `roostctl` round-trip needed. Each banner gets a unique identifier (tab id + timestamp), so repeated notifications on the same tab stack in Notification Center rather than replacing one another.
   - Linux: `org.freedesktop.Notifications` on the session bus (the Desktop Notifications spec — GNOME, KDE, COSMIC, and other spec servers). The GTK UI talks to that bus through `gio.Notification` (`app.focus-tab` as the default action). The iced UI talks to the same bus directly (`default` action + `ActionInvoked`). The notification id is fixed per tab, so a new banner *does* replace the previous one for that tab. Click-to-focus is the spec default action → focus that tab, clear its badge, reveal the sidebar, and best-effort raise the window (Wayland raise still needs an `xdg-activation` token iced 0.14 cannot consume).

   The macOS bullet above describes the Swift app. Roost-Iced on macOS (experimental) uses `UNUserNotificationCenter` too, but with a stable per-tab identifier rather than a unique one per event: a new notification for a tab **replaces** that tab's live banner instead of stacking another one in Notification Center — matching the Linux behavior above, not the Swift stacking. One live banner per tab, and a new event updates it in place. Banner clicks focus the tab while the app is running; a banner clicked after the app has quit and relaunched is not routed (unlike the Swift app).

## Focus policy

One predicate governs the badge, the inbox row, and the banner together:

```text
suppress := window is active AND the target tab is the active tab
```

When it holds, the notification is dropped **before** it's recorded anywhere — no badge, no inbox row, no banner fires. This is decided once, in the workspace, at the moment the notification arrives (the attention branch inside `Workspace::raise_attention` / `apply_agent_reports` on Linux, the mirrored path on Mac) — not independently by each surface, and not as a UI-side filter applied after the fact. That is what makes "switch away afterward" safe: it does not retroactively produce a badge, because nothing was ever recorded to begin with.

The three surfaces are coupled on purpose. Inbox membership *is* the tab's pending-notification bit — the same `has_notification` flag that drives the badge — so "suppress the banner and badge but keep the inbox row" isn't expressible without tracking notification history separately from pending state. That's a durable-notification-history feature, and it's explicitly out of scope; rather than smuggle half of it in, the three stay coherently coupled: a notification for the tab you are actively looking at is considered seen.

The **sticky agent-state indicator is the one exception** — it is never focus-cleared. That is what covers "I walked away and came back": even though the badge and inbox row for an earlier notification are long gone, the dot still shows `needs_input` (or the failed/red state) for a tab that wants you.

## Triaging across projects

Pending notifications are also a navigable list, so you can jump straight to what needs attention instead of hunting through the sidebar:

- **Jump to unread** (`Cmd-Shift-U` / `Alt-Shift-U`) focuses the next tab with a pending notification — the active project first, then the others. Focusing the tab clears its badge, so repeating the shortcut walks through everything pending.
- **The notification inbox** is the command palette's **View Notifications** entry: one row per pending tab, labeled `<project> · <tab>` with the message body. Activating a row jumps to that tab and clears it.
- **Clear All Notifications** (also in the command palette) empties the inbox and drops every pending badge at once.
- **Sidebar reveal.** Every "jump to a tab" path — jump-to-unread, activating a notification-inbox row, and activating a row in the agent palette (`Cmd-Shift-O` / `Alt-Shift-O`; see [Keybindings](../getting-started/keybindings.md)) — reveals the projects sidebar if it was collapsed, on **both** Mac and GTK. The reveal only happens once the jump actually succeeds (the target tab still exists); a jump at a since-closed tab leaves the sidebar exactly as it was.

## Tab targeting

`roostctl` resolves the target tab in this order:

1. The `--tab <id>` flag, if provided
2. The `ROOST_TAB_ID` environment variable, set by Roost when it spawns each tab's shell
3. Error: tab id required

Roost injects these environment variables into every spawned shell:

| Variable           | Value                                                              |
|--------------------|--------------------------------------------------------------------|
| `ROOST_TAB_ID`     | The integer tab id this shell is bound to                          |
| `ROOST_SOCKET`     | The Unix-socket path the GUI is listening on                       |

So `roostctl` invoked from inside any tab needs no flags or config — it knows where to send and which tab to mark.

## Hook-session OSC suppression

When a live agent session owns a tab — a Claude Code session driving it via `roostctl claude-hook`, or any other source that has claimed ownership through `tab.agent_report` — raw OSC 9 / 99 / 777 notifications from *inside* that tab are dropped. Hooks are the trusted channel: the owning agent already reports its own attention through structured events, so letting its raw OSC noise through on top would double-notify for the same thing. Structured notifications (`notification.create` / `roostctl notify`) are **never** suppressed this way — gating an agent's own trusted channel would mute the very thing the suppression exists to protect; only the raw-OSC fallback path is gated.

Ownership is normally claimed by a `SessionStart` hook and released by a matching `SessionEnd`. But an agent can also disappear with no `SessionEnd` — killed, crashed, `Ctrl-C`'d out from under the shell — which would otherwise mute the tab's raw OSC forever. The failsafe: an OSC 133 `D` (command end) or `A`/`B` (prompt) mark drops the tab's agent lifecycle to inactive while leaving ownership in place as a label. A prompt mark only fires once the shell reaches it, which means whatever the agent was running has exited — so it's a safe trigger. With the lifecycle inactive, derivation falls through to the shell state and raw OSC re-opens immediately: a dead agent degrades to "the tab still remembers `claude` used to own it" cosmetically, instead of silently swallowing every notification from then on.

## Manual override (`tab set-state`)

`roostctl tab set-state --tab N --state STATE` claims ownership as `manual` (an empty session id), which **supersedes whatever currently owns the tab** — if Claude is mid-turn, its subsequent hook events are dropped until its next `SessionStart`. That's intentional: running the command means you're taking the wheel.

```bash
roostctl tab set-state --tab 3 --state needs_input
roostctl tab set-state --tab 3 --state idle
```

`--state none` is the one case that **releases** ownership rather than claiming an inactive one, so the tab falls back to shell-derived state instead. Concretely: a tab with a live foreground process now shows `running` under `--state none`, not unconditionally `none` — "no state" means "no agent owns this tab," not "force the indicator blank."

## Examples

From inside a Roost tab:

```bash
roostctl notify --title "Build done" --body "tests pass"
```

From outside Roost, target a specific tab:

```bash
ROOST_SOCKET="$HOME/Library/Caches/roost/roost.sock" \
  roostctl notify --title "From CI" --body "deploy ready" --tab 3
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

- Body length is capped at 1 MiB on the OSC parser to bound buffer growth on a misbehaving sender. Longer bodies are truncated.
- On Wayland, `gtk.Window.Present()` without an XDG-activation token may only flash the taskbar instead of raising. Click-from-banner paths typically pass a token through; CLI scripts that call `roostctl tab focus` directly may not.

See the [Claude Code Hooks](claude-code.md) guide for the most common Claude Code wire-up.
