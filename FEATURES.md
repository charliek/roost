# Roost — Feature Backlog

A working list of features to consider for Roost, ranked by value to the
product's stated differentiator: a multi-project terminal multiplexer for
AI coding agents. This is a reference document, not a roadmap — items
here are candidates, not commitments.

See `docs/development/spec.md` for the original design rationale and
`docs/reference/architecture.md` for what already exists.

## How items are scored

- **Value** — how much it advances the multi-project / AI-agent
  differentiator, vs. generic terminal polish.
- **Size** — rough calendar effort. **S** = under a day, **M** = 1–3
  days, **L** = a week or more.
- **Complexity** — design risk, not lines of code. *Low* = fits existing
  patterns; *Med* = one or two real decisions; *High* = architectural
  thinking or novel territory.

## Ranked backlog

| # | Feature | Value | Size | Complexity | Notes |
|---|---|---|---|---|---|
| 1 | **Task tabs (use the reserved `tab.command` column).** Saved launch profiles for `claude`, `codex`, test watchers, deploy monitors. Auto-launch on tab open and on Roost restart. | High | M–L | Med | Schema already reserves it. Most natural product extension; this is what makes Roost feel built-for-agents instead of "tabs that happen to host agents." |
| 2 | **Notification severity levels** (`waiting-on-input` / `done` / `failed`). Different colors, urgencies, optional sounds. | High | S–M | Low | Cheap. Turns the badge from "something happened" into "act now vs. glance later" — exactly the signal a multi-agent setup needs. |
| 3 | **Richer `roost-cli` / automation API.** Add `new-tab`, `focus-tab`, `list-tabs`, `send`, `wait-idle`, `mark-done`, `open --project foo --cwd .`. | High | M (S per verb) | Low | IPC plumbing exists; each verb is small. Multiplies what hooks and external scripts can do. Pairs with #1 ("open a Claude task tab in this repo"). |
| 4 | **Command palette / quick switch (Cmd/Ctrl-P).** Fuzzy across `<project> / <tab title> / recent notifications`. | High | M | Low | `Cmd-1..9` doesn't scale past ~9. The navigation primitive that lets Roost host dozens of agents without becoming unusable. |
| 5 | **Per-project notification badge on the sidebar row** (count of tabs needing attention). | High | S | Low | A collapsed/unfocused project hides which tab is calling. One small badge fixes that. Direct multiplier on existing notifications. |
| 6 | **Notification history / inbox.** Pull-down panel listing recent notifications with timestamps, unread/resolved state, "jump to tab." | High | M | Low | Ephemeral badges lose information once you click away. Triage gets painful past ~3 active agents. |
| 7 | **Pin manual tab titles against OSC 1/2 overwrite.** Per-tab lock flag, or `display_name` separate from `shell_title`. | High | S | Low | Already documented as a known papercut in `docs/getting-started/keybindings.md`. Tiny change, daily payoff. |
| 8 | **Use existing `last_active` + `last_command` for "recent tabs," "show last command," "reopen last agent."** | High | S | Low | Columns already exist and are written. Nearly free UX wins. |
| 9 | **Find in scrollback (Cmd/Ctrl-F overlay).** | High | M | Med | Agent transcripts are long. Finding "where I told it no" is currently manual scrolling. |
| 10 | **Per-tab agent identity / status badge.** Detect Claude/Codex/etc. (injected env var, `roost-cli identify --as ...`, or proc scan) and show a small icon on the tab. | High | M | Med | Combined with #2 and #5, makes a strip of tabs scannable at a glance. |
| 11 | **Drag-to-reorder projects (sidebar) and tabs (within and between projects).** | High | S–M | Med | Positions are persisted but not user-rearrangeable. GTK4 DnD is fiddly but mechanical. |
| 12 | **Quiet hours / per-project mute** (right-click → mute, optionally "until X"). | High | S | Low | Prevents the "8 agents finish at midnight" pile-up. Trivial: one flag in `handleNotification`. |
| 13 | **Light git awareness on the project row** (current branch, dirty marker). Read-only; no worktree management. | High | S–M | Low | Spec Open Question #5; for an AI-agent tool the answer is clearly yes. Shell out to `git` on focus change/timer. |
| 14 | **Crash isolation around the libghostty surface.** Recover panics per session; show "session crashed — click to restart." | High | M | Med | Listed for Phase 3 in the spec. Long-running agent sessions make this matter more than for a normal terminal. |
| 15 | **OSC 8 hyperlinks** (clickable, route `file://` to `$EDITOR`). | Med–High | M | Med | Agents increasingly emit linkified file refs and doc links. |
| 16 | **OSC 133 prompt marks + jump-to-prompt.** | Med | M | Med | Lets you navigate by turn boundary in long agent transcripts. Only kicks in once shells/agents emit the marks. |
| 17 | **Preferences UI** (font, keybindings, default shell, paste limit, notification behavior, theme picker). | Med | M | Low | Theme switching exists via `config.conf`; a UI picker is the missing surface. Removes a recurring papercut. |
| 18 | **User theme override directory + hot-reload.** Currently only the bundled themes ship and theme load is restart-only. Drop-in `~/.config/roost/themes/` and reload-on-change would close the loop with `~/.config/ghostty/themes/`. | Med | S | Low | Theme parser already accepts ghostty-format files (see `docs/reference/themes.md`). |
| 19 | **Drag-and-drop file → quoted path paste.** | Med | S | Low | Constantly useful when handing files to agents. |
| 20 | **Scrollback persistence across shell exit / Roost restart.** | Med | L | High | Documented as "lost on shell exit," which hurts when an agent crashes. Replaying terminal state correctly is genuinely hard — defer until #14 isn't enough. |
| 21 | **Inline image protocols (Kitty graphics, iTerm2; Sixel last).** | Med | L | High | Some agents are starting to emit images; without this Roost prints garbage. Depends on what libghostty-vt exposes. |
| 22 | **Distribution: notarized `.dmg` + Homebrew tap, AppImage/Flatpak, auto-update, crash reporting.** | Med (high once you want users) | L | Low (mostly drudgery) | Spec already calls this "a few weekends of unfun work." Gates anyone but you running it. |

## If you only ship four

**#1 task tabs**, **#2 severity**, **#3 richer CLI**, **#4 palette.**
Those are the four where the product *shape* moves — and #2/#3 are
mostly small mechanical work that opens up larger payoffs (#1 needs a
launch profile, #6 needs richer notifications) so doing them first makes
the rest cheaper.

## Explicit non-goals

These come up naturally but are out of scope by design. Restating them
here so they don't drift into the backlog:

- **Split panes** — one terminal per tab. Notification routing gets
  weaker the more places output can land.
- **Multi-window** — one window, projects in sidebar, tabs in projects.
- **Embedded browser / webview shell.**
- **Git worktree management** (cmux feature, intentionally out of MVP).
- **Windows support.**
- **General plugin system** — too early. Wait until two real extensions
  want the same surface area.
