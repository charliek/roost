# Testing Claude Code integration + tab state + notifications

This doc maps the end-to-end UI surface a Claude Code (or any
agent) session lights up in Roost, plus the exact CLI commands you
can drive from a sibling shell to exercise each path. Use it to
verify any UI change in the area, or to demo the integration.

## What the UI shows

| Surface | What it means | Where it lives |
|---|---|---|
| **Pill dot** (10pt circle, leading edge of a tab pill) | The tab's agent state. Color encodes which state. | `TabPillView.statusSlot` (Mac); Linux uses an `Adw.TabPage` icon. |
| **Sidebar stripe** (3pt vertical band on the leading edge of a project row) | The *rollup* of all tabs in the project's agent states. Priority: `needs_input > running > idle > none`. Tabs with `hook_active=true` are SKIPPED in the rollup math (Claude owns the urgency signal on those). | `ProjectRowCellView.stripe` (Mac); Linux uses a CSS class on the row. |
| **Tab pill badge dot** (8pt accent circle, trailing edge of inactive notified pills) | The tab has a pending notification. Cleared when the user focuses the tab. | `TabPillView.badgeDot` (Mac). |
| **Project row badge dot** (sidebar trailing-edge dot on a project row) | At least one tab in this project has a pending notification. Same focus-clear behavior. | `ProjectRowCellView.badgeDot` (Mac). |
| **Desktop banner** | A macOS banner (UNUserNotificationCenter) with title + body. Clicking it brings Roost to front and focuses the originating tab. | `DesktopNotifications` (Mac); Linux uses `NotificationCenter`. |

## State model

| State | Pill dot color | Set via CLI | Triggered by Claude hook |
|---|---|---|---|
| `none` | no dot | `tab set-state --state none --tab N` | `session-end` |
| `running` | blue (`#5fa3f0`) | `tab set-state --state running --tab N` | `prompt-submit` |
| `needs_input` | amber (`#f0a040`) | `tab set-state --state needs_input --tab N` | `notification` |
| `idle` | gray (`#7a7a7a`) | `tab set-state --state idle --tab N` | `stop` |

## CLI cheatsheet

Pre-req: `roostctl tab list` to find a tab id. Either `export ROOST_TAB_ID=<id>` or pass `--tab <id>` explicitly to each command. (When a shell is running inside a Roost tab, `ROOST_TAB_ID` is set automatically.)

| Command | Effect |
|---|---|
| `roostctl tab list` | Print all tabs grouped by project + their current state. |
| `roostctl tab set-state --state STATE --tab N` | Set state. `STATE ∈ {none, running, needs_input, idle}`. |
| `roostctl notify --title "Hi" --body "..." --tab N` | Fire a desktop banner + set the pill badge. |
| `roostctl tab clear-notification --tab N` | Clear the pill badge (state unchanged). |
| `roostctl tab focus --tab N` | Equivalent to clicking the pill; clears the badge as a side effect. |
| `roostctl screenshot --out /tmp/shot.png` | Render the whole window to a PNG in-process (no OS screen capture) — read it back to *see* the UI state you just drove. Add `--scale 2` for a crisper image. |
| `ROOST_TAB_ID=N roostctl claude-hook session-start` | Engages `hook_active` suppression on the tab. (OSC 9/777 from the shell becomes a no-op; only `create-notification` RPCs emit banners.) |
| `echo '{"message":"need input"}' \| ROOST_TAB_ID=N roostctl claude-hook notification` | Sets `needs_input` + fires "Claude Code" banner. |
| `ROOST_TAB_ID=N roostctl claude-hook stop` | Sets `idle` + fires "Turn complete" banner. |
| `ROOST_TAB_ID=N roostctl claude-hook session-end` | Releases `hook_active` + sets `none`. |
| `ROOST_TAB_ID=N roostctl claude-hook prompt-submit` | Sets `running` + clears pending notification. |

## Test checklist

### T1 — state color progression

1. `tab set-state --state idle --tab N`
   → pill dot gray; sidebar stripe gray (if no higher-priority tab in the project).
2. `tab set-state --state running --tab N`
   → pill dot blue; sidebar stripe blue.
3. `tab set-state --state needs_input --tab N`
   → pill dot amber; sidebar stripe amber.
4. `tab set-state --state none --tab N`
   → pill dot disappears; sidebar stripe reflects the next-highest state in the project (or hides).

### T2 — notification banner + per-tab badge

Pre-req: focus a *different* tab in the same project so the test tab is inactive — badges only show on inactive pills.

1. `notify --title "Test" --body "Body" --tab N`
   → macOS banner top-right (title "Test", body "Body");
   pill N grows a small accent badge dot on the trailing edge.
2. Click the banner.
   → Roost activates, tab N becomes focused, badge dot vanishes (focus-clears).
3. Re-fire `notify`, then `tab clear-notification --tab N`.
   → Badge clears without focusing. State stays whatever it was.

### T3 — hook suppression + sidebar rollup

1. With 2+ tabs in a project, set Tab A `running` and Tab B `needs_input`.
   → Sidebar stripe = amber (`needs_input` wins).
2. `ROOST_TAB_ID=<Tab B id> roostctl claude-hook session-start`.
   → Sidebar stripe drops to **blue** (Tab B's `needs_input` is now suppressed in rollup; Tab A's `running` becomes max).
3. `ROOST_TAB_ID=<Tab B id> roostctl claude-hook session-end`.
   → Stripe back to amber. Tab B's state goes to `none`.

### T4 — project-row badge (separate from per-tab badge)

1. With Tab A in Project P notified (`tab set-state --state needs_input --tab A`,
   `notify --tab A ...`), focus a tab in a *different* project.
   → Project P's sidebar row shows an accent badge dot AND its stripe is amber.
2. Click Tab A (or focus from CLI).
   → Tab A's pill badge + Project P's sidebar row badge both clear. Stripe stays amber (state unchanged).

### T5 — end-to-end Claude lifecycle simulation

1. `ROOST_TAB_ID=N roostctl claude-hook session-start`.
   → No visible change (Claude hook engages silently).
   → Internally: `hook_active=true` so OSC 9/777 from the shell is now suppressed.
2. `ROOST_TAB_ID=N roostctl claude-hook prompt-submit`.
   → Pill dot blue; sidebar stripe blue (no other tabs with higher-priority state).
   → Any prior pending notification is cleared.
3. `echo '{"message":"choose a path"}' | ROOST_TAB_ID=N roostctl claude-hook notification`.
   → Pill dot amber; banner "Claude Code: choose a path";
     sidebar stripe NOT updated (hook-active demotes this tab in rollup).
4. Click the banner → focuses Tab N. Pill badge clears.
5. `ROOST_TAB_ID=N roostctl claude-hook stop`.
   → Pill dot gray; banner "Claude Code: Turn complete".
6. `ROOST_TAB_ID=N roostctl claude-hook session-end`.
   → Pill dot disappears; sidebar stripe drops to next-highest-priority tab in the project (or hides).

### T6 — UI log inspection

There is no shared daemon. Watch the running UI's log while driving the
above tests:

```bash
# macOS (Swift Roost.app)
tail -f ~/Library/Logs/Roost/roost.log

# Linux (gtk4-rs roost) — also tees to stdout
tail -f "${XDG_STATE_HOME:-$HOME/.local/state}/roost/roost.log"
```

Each CLI command above lands as a corresponding log entry —
`set_tab_state`, `set_hook_active`, `tab_notification`,
`create_notification`. If the UI doesn't react to an expected
event, the log line tells you whether the UI received the
IPC request at all.

### T7 — visual verification via screenshot

Instead of (or alongside) reading the log, capture the live UI as a PNG
and inspect it directly. The UI renders its own window in-process, so
this works even when the window is unfocused or behind other windows —
no OS screen-capture permission needed.

1. Drive a visible change, e.g. `tab set-state --state needs_input --tab N`.
2. `roostctl screenshot --out /tmp/roost.png` (add `--scale 2` for a
   crisper image; target a specific UI with `--target mac` / `--target gtk`).
3. Open `/tmp/roost.png` — confirm the pill dot color, sidebar stripe,
   and badge match what the state change should produce.

This is the fastest way for an automated agent to *see* the result of a
UI edit rather than infer it from log lines.

## Permanent hook setup (Claude Code)

To wire the actual Claude Code CLI so it drives these events
automatically when you run a session:

```bash
roostctl claude install
```

This writes `~/.config/roost/claude-settings.json` with hook
commands for each lifecycle event, then prints an alias line:

```bash
alias claude='claude --settings ~/.config/roost/claude-settings.json'
```

Add that alias to your shell rc. Now every `claude` session
inside a Roost tab automatically drives the integration:

- Start of session → `claude-hook session-start` (engages hook_active).
- Each prompt submission → `claude-hook prompt-submit` (state=running).
- Claude needs input (e.g. tool approval) → `claude-hook notification` (state=needs_input + banner).
- Claude finishes a turn → `claude-hook stop` (state=idle + "Turn complete" banner).
- End of session → `claude-hook session-end` (releases hook_active).
