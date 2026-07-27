# Testing Claude Code integration + tab state + notifications

This doc maps the end-to-end UI surface a Claude Code (or any
agent) session lights up in Roost, plus the exact CLI commands you
can drive from a sibling shell to exercise each path. Use it to
verify any UI change in the area, or to demo the integration.

## What the UI shows

| Surface | What it means | Where it lives |
|---|---|---|
| **Pill dot** (10pt circle, leading edge of a tab pill) | The tab's *effective* agent state — blue (running), orange (needs-input), gray (idle), red (failed), or none. Derived from the shell axis (OSC 133 marks) when no agent owns the tab, or the owning agent's own lifecycle when one does. | `TabPillView.statusSlot` (Mac); Linux uses an `Adw.TabPage` icon. |
| **Sidebar stripe** (3pt vertical band on the leading edge of a project row) | The *rollup* of all tabs in the project's effective states. Ranked `failed > needs_input > running > idle` (`crates/roost-ipc/src/agent.rs::rank`, shared with the per-tab dot). Every tab participates, including one a hook currently owns — a project whose only blocked tab is a live Claude session gets a stripe, not silence. | `ProjectRowCellView.stripe` (Mac); Linux uses a CSS class on the row. |
| **Tab pill badge dot** (8pt accent circle, trailing edge of inactive notified pills) | The tab has a pending notification. Cleared when the user focuses the tab. | `TabPillView.badgeDot` (Mac). |
| **Project row badge dot** (sidebar trailing-edge dot on a project row) | At least one tab in this project has a pending notification. Same focus-clear behavior. | `ProjectRowCellView.badgeDot` (Mac). |
| **Desktop banner** | A macOS banner (`UNUserNotificationCenter`) with title + body. Clicking it brings Roost to front and focuses the originating tab. | `DesktopNotifications` (Mac); Linux uses `gio.Notification`. |

## State model

The dot/stripe color is a **derived** projection of independent axes — shell state (OSC 133 marks) and agent lifecycle (agent adapters) — not a single field any of the old three writers set directly. Full wire-level detail is in [`ipc.md`](../reference/ipc.md).

| Effective state (dot color) | Driven by | Manual CLI |
|---|---|---|
| none (no dot) | no live agent + shell `at_prompt`/`unknown` | `tab set-state --state none --tab N` (**releases** ownership — see below) |
| running (blue) | agent lifecycle `working`, **or** no agent + shell `foreground_process` | `tab set-state --state running --tab N` |
| needs_input (orange) | agent lifecycle `waiting` | `tab set-state --state needs_input --tab N` |
| idle (gray) | agent lifecycle `finished` | `tab set-state --state idle --tab N` |
| failed (red) | agent lifecycle `failed` (collapses to `needs_input` on the legacy wire `state` field, but renders as its own color in the UI) | not reachable via `tab set-state` (its accepted values are `{none, running, needs_input, idle}`) — drive it with a `StopFailure` hook payload (T5b below) or a raw `tab.agent_report` |

`tab set-state` claims ownership as `manual`, which **supersedes** whatever currently owns the tab, a live Claude session included — see [Notifications → Manual override](../guides/notifications.md#manual-override-tab-set-state). Claude's own hook → state mapping is documented in full in [Claude Code Hooks](../guides/claude-code.md); this doc's CLI cheatsheet below reproduces just enough of it to drive by hand.

## CLI cheatsheet

Pre-req: `roostctl tab list` to find a tab id — add `--json` to see the full agent record (`shell_state` / `agent_lifecycle` / `ownership`); the plain-text form only prints the legacy 4-value `state`. Either `export ROOST_TAB_ID=<id>` or pass `--tab <id>` explicitly to each command. (When a shell is running inside a Roost tab, `ROOST_TAB_ID` is set automatically.)

| Command | Effect |
|---|---|
| `roostctl tab list` / `roostctl tab list --json` | Print all tabs grouped by project + their state (plain), or the full `Tab` record including the agent axes (`--json`). |
| `roostctl tab set-state --state STATE --tab N` | Set state. `STATE ∈ {none, running, needs_input, idle}`. Claims ownership as `manual`; supersedes a live agent. |
| `roostctl notify --title "Hi" --body "..." --tab N` | Fire a desktop banner + set the pill badge. Never suppressed by hook ownership — only the raw-OSC path is gated. |
| `roostctl tab clear-notification --tab N` | Clear the pill badge (state unchanged). |
| `roostctl tab focus --tab N` | Equivalent to clicking the pill; clears the badge as a side effect. |
| `roostctl screenshot --out /tmp/shot.png` | Render the whole window to a PNG in-process (no OS screen capture) — read it back to *see* the UI state you just drove. Add `--scale 2` for a crisper image. |

`roostctl claude-hook EVENT` works two ways when driven by hand.

**With no stdin at all** — `ROOST_TAB_ID=N roostctl claude-hook session-start` — `roostctl` synthesizes a deterministic `session_id` of `manual:<tab id>`, so a bare sequence is self-consistent: the `SessionStart` claim and the `SessionEnd` release carry the same identity. This is the quickest way to walk the lifecycle by hand.

**With a payload**, which is what Claude Code itself sends, carry a **`session_id`** and repeat the *same* one for every later event. All events but `SessionStart`/`SessionEnd` `preserve` ownership, which matches on the `(source, session_id)` pair, so a mismatch is silently dropped — that is the mechanism keeping a stale session from talking over a live one.

A `SessionStart` whose payload carries an *empty* `session_id` is dropped rather than claiming: a claim supersedes unconditionally, so an event that cannot identify its own session must not evict whatever already owns the tab. (The synthesized `manual:<tab id>` above is why the payloadless form is not affected by this.)

Pick any non-empty string and reuse it for the whole sequence:

| Command | Effect |
|---|---|
| `echo '{"session_id":"t1"}' \| ROOST_TAB_ID=N roostctl claude-hook session-start` | Claims ownership as `claude`/`t1`; lifecycle → inactive. No visible dot change yet, but raw OSC 9/99/777 is now suppressed on this tab. |
| `echo '{"session_id":"t1"}' \| ROOST_TAB_ID=N roostctl claude-hook prompt-submit` | Lifecycle → working (blue dot); clears any pending notification. |
| `echo '{"session_id":"t1","notification_type":"permission_prompt","message":"choose a path"}' \| ROOST_TAB_ID=N roostctl claude-hook notification` | Lifecycle → waiting (orange dot); fires a warn banner "Claude Code: choose a path". |
| `echo '{"session_id":"t1"}' \| ROOST_TAB_ID=N roostctl claude-hook stop` | No `background_tasks` → lifecycle → finished (gray dot); fires an info banner "Turn complete". Add `"background_tasks":[{"id":"1","type":"shell","status":"running","description":"build"}]` to the payload and lifecycle stays `working` instead — this is how Roost tells "done" apart from "paused, waiting on background work." |
| `echo '{"session_id":"t1","error":"rate_limit"}' \| ROOST_TAB_ID=N roostctl claude-hook stop-failure` | Lifecycle → failed (red dot); fires an error banner naming the error. |
| `echo '{"session_id":"t1"}' \| ROOST_TAB_ID=N roostctl claude-hook session-end` | Releases ownership; lifecycle → inactive; clears any pending notification. Tab falls back to shell-derived state. |

Both the canonical `hook_event_name` spelling Claude Code itself sends (`SessionStart`, `UserPromptSubmit`, `Notification`, `Stop`, `StopFailure`, `SessionEnd`) and the kebab-case spelling used above (what `roostctl claude install` wrote before this event set existed) are accepted — see [Claude Code Hooks](../guides/claude-code.md).

## Test checklist

### T1 — state color progression

1. `tab set-state --state idle --tab N`
   → pill dot gray; sidebar stripe gray (if no higher-priority tab in the project).
2. `tab set-state --state running --tab N`
   → pill dot blue; sidebar stripe blue.
3. `tab set-state --state needs_input --tab N`
   → pill dot amber; sidebar stripe amber.
4. `tab set-state --state none --tab N`
   → releases ownership. If the tab's shell is at a prompt, the dot disappears; if a foreground process happens to be live in the tab, the dot instead shows running (blue) — `none` falls through to shell state rather than forcing the dot blank. Sidebar stripe reflects the next-highest state in the project (or hides).

(`failed`/red isn't reachable from `tab set-state` — see T5b.)

### T2 — notification banner + per-tab badge

Pre-req: focus a *different* tab in the same project so the test tab is inactive — badges only show on inactive pills.

1. `notify --title "Test" --body "Body" --tab N`
   → macOS banner top-right (title "Test", body "Body");
   pill N grows a small accent badge dot on the trailing edge.
2. Click the banner.
   → Roost activates, tab N becomes focused, badge dot vanishes (focus-clears).
3. Re-fire `notify`, then `tab clear-notification --tab N`.
   → Badge clears without focusing. State stays whatever it was.

### T3 — hook ownership participates in the sidebar rollup

Before plan 002 a tab a hook owned was **excluded** from the rollup math entirely, so a project whose only blocked tab was a live Claude session showed no stripe at all. This checks the fix.

1. In a project with a single tab, claim it as Claude and put it in `needs_input`:
   ```bash
   echo '{"session_id":"t1"}' | ROOST_TAB_ID=<tab id> roostctl claude-hook session-start
   echo '{"session_id":"t1","notification_type":"permission_prompt","message":"go ahead?"}' \
     | ROOST_TAB_ID=<tab id> roostctl claude-hook notification
   ```
   → Sidebar stripe is **amber** — the Claude-owned tab's `needs_input` is visible, not hidden.
2. Add a second, plain-shell tab to the same project with a foreground process running (`running`, blue).
   → Stripe stays amber — `needs_input` still outranks `running`.
3. `echo '{"session_id":"t1"}' | ROOST_TAB_ID=<tab id> roostctl claude-hook session-end` on the first tab.
   → Stripe drops to blue (only the plain-shell tab's `running` remains).

### T4 — project-row badge (separate from per-tab badge)

1. With Tab A in Project P notified (`tab set-state --state needs_input --tab A`,
   `notify --tab A ...`), focus a tab in a *different* project.
   → Project P's sidebar row shows an accent badge dot AND its stripe is amber.
2. Click Tab A (or focus from CLI).
   → Tab A's pill badge + Project P's sidebar row badge both clear. Stripe stays amber (state unchanged).

### T5 — end-to-end Claude lifecycle simulation

Uses a fixed `session_id` (`t1`) across every step — see the CLI cheatsheet above for why that matters.

1. `echo '{"session_id":"t1"}' | ROOST_TAB_ID=N roostctl claude-hook session-start`.
   → No visible dot change. Internally: ownership claimed by `claude`/`t1`, so raw OSC 9/99/777 from the shell is now suppressed on this tab.
2. `echo '{"session_id":"t1"}' | ROOST_TAB_ID=N roostctl claude-hook prompt-submit`.
   → Pill dot blue; sidebar stripe reflects it if it's the highest-ranked tab in the project.
   → Any prior pending notification is cleared.
3. `echo '{"session_id":"t1","notification_type":"permission_prompt","message":"choose a path"}' | ROOST_TAB_ID=N roostctl claude-hook notification`.
   → Pill dot amber; banner "Claude Code: choose a path"; sidebar stripe **reflects** this tab if it's the project's highest-ranked one (T3 — hook ownership no longer hides it from the rollup).
4. Click the banner → focuses Tab N. Pill badge clears.
5. `echo '{"session_id":"t1"}' | ROOST_TAB_ID=N roostctl claude-hook stop`.
   → Pill dot gray; banner "Claude Code: Turn complete".
6. `echo '{"session_id":"t1"}' | ROOST_TAB_ID=N roostctl claude-hook session-end`.
   → Pill dot disappears (or falls back to whatever the shell's own state is); sidebar stripe drops to the next-highest-priority tab in the project (or hides).

### T5b — `StopFailure` and the OSC 133 failsafe

1. Repeat T5 steps 1–2 to get the tab owned and running.
2. `echo '{"session_id":"t1","error":"rate_limit","error_details":"Rate limited, retry later"}' | ROOST_TAB_ID=N roostctl claude-hook stop-failure`.
   → Pill dot **red**; error banner "Claude Code: Rate limited, retry later". If another tab in the same project is merely `needs_input`, the stripe stays on this tab's color — `failed` outranks `needs_input`.
3. Without a `session-end`, get the shell to a fresh prompt (press enter on an empty line in the tab is the real trigger — an OSC 133 `A`/`D` mark).
   → Lifecycle drops to inactive; the dot falls back to shell state, and raw OSC 9/99/777 is no longer suppressed on this tab. This is the failsafe against a killed/crashed agent muting a tab forever — see [Notifications → Hook-session OSC suppression](../guides/notifications.md#hook-session-osc-suppression).

### T6 — UI log inspection

There is no shared daemon. Watch the running UI's log while driving the
above tests:

```bash
# macOS (Swift Roost.app)
tail -f ~/Library/Logs/Roost/roost.log

# Linux (gtk4-rs roost) — also tees to stdout
tail -f "${XDG_STATE_HOME:-$HOME/.local/state}/roost/roost.log"
```

If the UI doesn't react to an expected command, this is the fastest
way to tell "the IPC request never arrived" from "it arrived but
didn't do what I expected" — correlate by timestamp against when you
ran the CLI command.

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

This writes `~/.config/roost/claude-settings.json` with hook commands
for each of the six lifecycle events — `SessionStart`,
`UserPromptSubmit`, `Notification`, `Stop`, `StopFailure`,
`SessionEnd` — then prints an alias line:

```bash
alias claude='claude --settings ~/.config/roost/claude-settings.json'
```

Add that alias to your shell rc. Now every `claude` session inside a
Roost tab automatically drives the integration — see
[Claude Code Hooks](../guides/claude-code.md) for the full event →
effect mapping, including the `background_tasks` / `StopFailure` /
unrecognized-`notification_type` cases the manual cheatsheet above
walks through by hand.
