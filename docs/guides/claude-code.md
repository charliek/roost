# Claude Code Hooks

Claude Code is one of five agents the [Agent Hooks](agents.md) guide
covers — that page is the mechanism (how wiring works, how to opt out,
the guarantee about what a merge does and doesn't touch, the
`roostctl agent` verbs), the remote behavior, and the troubleshooting
path shared by all five. This page is what's specific to Claude: three
notes worth knowing plus the day-one verification walk.

Nothing to install by hand: Roost wires Claude's hooks itself the first
time the UI starts (`agent-hooks = auto` in `config.conf`, the default).
See [Agent Hooks → Install](agents.md#install) to do it manually, and
[Agent Hooks → How to opt out](agents.md#how-to-opt-out) to turn it off.

## Three things specific to Claude

**`PermissionRequest` vs. the 6-second `permission_prompt` notification.**
Claude actually fires two different blocked signals, and Roost only
needs the first one. `PermissionRequest` fires the instant the approval
dialog opens — that's the immediate, reliable "needs input" signal.
Separately, a `Notification` with `notification_type: permission_prompt`
fires roughly 6 seconds later if the dialog is still open (cancelled the
moment you answer it). Because `PermissionRequest` already moved the tab
to `waiting`, that second notification is guarded
(`lifecycle_if: [working]`) so it fires vetoed rather than banner you
twice for the same prompt — it still does the job on an older,
hand-installed settings file that has no `PermissionRequest` hook at
all.

**`idle_prompt` is the only signal after an Esc interrupt, and it's
guarded.** Claude has no interrupt hook — pressing Esc mid-turn produces
no event of its own. The one later signal is `idle_prompt`, a
`Notification` that fires about 60 seconds after a turn goes quiet. It's
guarded to `lifecycle_if: [working]` so it can only ever move a tab from
"still working" to "finished" — a genuinely `waiting` or `failed` tab is
left alone, and (as of this release) it no longer banners "Turn
complete" for a session that already reported finished by other means;
see the changelog if you're used to seeing that banner every time.

**`background_tasks` distinguishes "done" from "paused, will resume."**
`Stop` with an empty `background_tasks` array means the turn is over —
gray dot, "Turn complete" banner. `Stop` with a *non-empty*
`background_tasks` keeps the tab at `working` instead, because the
session isn't actually idle: it'll wake back up when that work finishes.
A scheduled `session_crons` entry alone does **not** count as in-flight
— only actual running/pending background work does.

Beyond those three, Claude also reports `PreToolUse`/`PostToolUse`
(keeps the dot blue through a tool call — and specifically, after you
approve a permission prompt, the dot returns to blue when the approved
tool *finishes*, not the instant you click Approve, since there's no
second `PreToolUse` to catch), `PermissionDenied`/`PostToolUseFailure`
(the turn keeps running), and `StopFailure` (red dot, naming Claude's
reported error). `roostctl claude-hook` also still accepts the
kebab-case event spellings an early Roost version wrote
(`session-start`, `prompt-submit`, `notification`, `stop`,
`session-end`), so a hand-installed settings file from before this
adapter existed keeps working across an upgrade.

## Verifying

Open a fresh Roost tab, source your rc if needed, then:

```bash
roostctl identify
```

You should see a handful of `key=value` lines (`socket`, `pid`, `active_tab`, `ui_version`, …) describing the running app — not JSON. If it errors, the GUI isn't running or `ROOST_SOCKET` is unset — re-launch `roost` and try again.

Now run `claude` and submit a prompt. Watch the tab indicator:

- **Running (blue)** while Claude is working.
- **Needs-input (orange)** if Claude asks for permission. A desktop banner fires; click it to focus the tab.
- **Idle (gray)** when the turn ends. A "turn complete" banner fires (unless background work is still in flight — then it stays running).
- **Failed (red)** if the turn ends in an error (`StopFailure`) — Claude hitting a rate limit, an auth failure, an overloaded model, etc.
- **No indicator** between sessions.

If a project has multiple tabs running Claude, the project's sidebar row picks up a left-edge stripe in the most actionable color across its tabs, ranked `failed > needs-input > running > idle`.

## Troubleshooting

Run `roostctl doctor` first. It's read-only, covers most of the cases
below in one pass, and names exactly which check is unhappy instead of
you guessing from the symptom — see [Agent Hooks →
Troubleshooting](agents.md#troubleshooting) for the full per-agent
`Agents` section.

- **Hooks don't fire** — check `roostctl doctor -v`'s `agent.hook_binary`
  check: a wrapper that strips the tab's environment (`env -i`, a `sudo`
  re-exec, a sanitizing launcher) makes the installed hook silently
  inert even though the settings file is correct. `claude.hook_events`
  and `claude.hook_command` confirm the settings file itself registers
  every event and resolves to a runnable `roostctl`; `claude.observed`
  says whether Roost has actually seen a hook fire on this tab.
- **Click-through doesn't focus** — on Linux, your notification daemon must support default actions (mako, dunst, GNOME Shell all do). On Wayland without an XDG-activation token, the window may only request attention rather than raise.
- **OSC 9 banners still appear from inside Claude** — that means the `SessionStart` hook didn't reach Roost (no ownership claimed, so raw OSC isn't suppressed). Doctor's `tab.raw_osc` check reports whether suppression is currently active on the tab; check `roostctl identify` and re-source your rc if it isn't.
- **The dot stopped moving after Claude errored out mid-session** — if Claude was killed or crashed without a `SessionEnd`, an OSC 133 prompt mark from the shell (reaching a fresh prompt) automatically releases the stale lifecycle and falls back to shell state; typing a command and pressing enter should clear it. See [Notifications → Hook-session OSC suppression](notifications.md#hook-session-osc-suppression) for the mechanism.

See [Agent Hooks](agents.md) for the mechanism every agent shares, and [Notifications](notifications.md) for the full pipeline architecture.
