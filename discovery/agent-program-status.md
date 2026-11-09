# Agent program status (OSC 7501): what each agent sends

**Captured 2026-10-28 on the Pop!_OS dev box, for plan 082 (protocol-7
release, M1b).** This is the blocking first step of
[protocol-7-release.md](protocol-7-release.md) § M1b. Re-run it with
`tools/agent-status/` (see its README) whenever an agent's 7501 support
changes.

Each agent ran in a pseudo-terminal that answers the common terminal
queries and answers `OSC 7501 ; ?` exactly as libghostty does once
`GHOSTTY_TERMINAL_OPT_PROGRAM_STATUS` is set:
`ESC ] 7501 ; ?:states=idle,working,done,blocked,error:kinds=permission,question,auth ST`.
Every `ROOST_*`, `CLAUDE*`, `CODEX_COMPANION_*`, `HERDR_*` and
multiplexer variable was removed from the agent's environment. (A nested Claude Code inherits the parent's
`CLAUDE_CODE_CHILD_SESSION` and runs with its permission mode, which
hides every permission prompt.)

Prompt text, model output and `msg`/`title` contents are left out. Where a
`msg` matters, its shape is described.

## Versions

| Agent | Version | Roost hook source | 7501 `app` |
|---|---|---|---|
| Claude Code | 2.1.312 (latest on 2026-10-28) | `claude` | `claude-code` |
| Codex | codex-cli 0.165.0 | `codex` | `codex` |
| Grok CLI (xAI) | 1.0.70 | `grok` | `grok-build` |
| gx (Charlie's grok-build fork) | 1.0.45+gx.17 | `grok` | none: no 7501 |
| cursor-agent | 2026.10.22-27dcf5f | `cursor` | none: no 7501 |
| opencode | 1.19.4 (npm latest) | `opencode` | none in this release. Source `dev` (`packages/tui/src/context/program-status.ts`, 2.2.0) sends `opencode` |
| crush | 0.98.2 (release tarball; apt still had 0.97.1) | none | `crush` |
| pi | 1.4.0 (`@earendil-works/pi-coding-agent`) | none | `pi` |

## What each sends

**Claude Code 2.1.312.** Probes first and stays silent if the probe gets no
reply.
- Startup: `idle`.
- A turn: `working` (no msg), then `working` with a one-line activity
  summary in `msg`, then `done`.
- A permission prompt: `blocked`, `kind=permission`, and `msg` is
  "approve <Tool>: <command>". Approving goes back to `working`.
- Esc mid-turn: `idle` within ~0.1 s.
- Exit (Ctrl-C twice): three `clear`s with an empty id.
- A background task (`sleep 25` started in the background): no child
  records. The turn ends `done`; when the task finishes, Claude runs a
  follow-up turn (`working` → `done`). No record carried an `id` in any
  capture.
- `title` was never sent.

**Claude Code's OSC 133 marks.** The binary emits `133;A;redraw=0` at turn
start and `133;C` + `133;D` at turn end, gated by
`h !== null && screenReaderMode && !fullscreen && !decstbmRenderer &&
terminal !== "WezTerm"`. So it fires **only in Screen Reader Mode**.
With `CLAUDE_AX_SCREEN_READER=1` the capture showed `working`, then
`133;A;redraw=0` 4 ms later, then `working` again 32 ms after that.

**Codex 0.165.0.** Does not probe; reports anyway. Terminator is ST.
- Startup: `idle`, `working`, `idle` within the first ~1 s (the startup
  flash).
- A turn: `working`, then `done`, then `idle` when the composer is empty
  again.
- An approval (run with `-s read-only -a on-request`): `blocked`,
  `kind=permission`, **no msg**. Approving goes back to `working`.
- Esc mid-turn: `idle`.
- Ctrl-Z: `clear` (empty id) before suspending. On SIGCONT/teardown it
  sent `idle` again.
- Exit: `clear`.

**Grok CLI 1.0.70.** Probes first.
- Startup in an untrusted folder: `blocked`, `kind=permission`,
  `msg` is a short trust question, within 0.1 s. Accepting gives `idle`.
- A turn: `working` with an activity `msg` (twice), then `done` with a
  short summary `msg`.
- Exit: `clear`.

**crush 0.98.2.** Does not probe. `idle` at startup, `working` on submit,
then `error` (that turn failed at the provider). Exit not captured
cleanly.

**pi 1.4.0.** Probes first. `idle` at startup, `clear` on exit (no
provider configured, so no turn).

**opencode** (source only). One root summary record plus one child
record per pending session (`id` = a hash of the session id): `working`
or `blocked` with `kind` and a clipped `msg`. The root summarises
(`N sessions working`). `clear` on cleanup.

**gx, cursor-agent, opencode 1.19.4.** No 7501 output and no probe.

## What this changes in the release doc's rules

1. **Source → app table (rule 5):** `claude`↔`claude-code`,
   `codex`↔`codex`, `grok`↔`grok-build`, `opencode`↔`opencode`.
   `cursor` maps to nothing. crush and pi have no Roost hooks. gx shares
   the `grok` source, so a gx claim in a tab whose record says
   `grok-build` is a same-app claim.
2. **The OSC 133 question is closed:** Claude's turn marks fire only in
   Screen Reader Mode. When they do, the `A` arrives just after the
   turn's first `working`, and Claude re-sends `working` tens of
   milliseconds later. Nothing in the bytes tells these marks from a
   shell's, so plan 082 treats them as prompt marks and pins the
   resulting sequences with tests (decided 2026-10-28).
3. **Also, "OSC 133 A" alone is the wrong trigger in Roost.** Roost's own
   bash and zsh integration
   (`crates/roost-engine/resources/shell-integration/roost.{bash,zsh}`)
   emits only `C` and `D`, never `A`. Only fish marks `A` itself. The
   existing hook failsafe already treats `A`, `B` and `D` as "back at a
   prompt" (`crates/roost-ipc/src/agent.rs:339-358`). Plan 082 uses the
   same set for rules 3 and 6.
4. **Child records:** none seen in these captures from the installed
   agents. opencode's source is the only producer, so the "highest-ranked live record" rule
   is covered by unit tests and an end-to-end test that feeds bytes.
5. **Banners at startup:** Grok's trust prompt is a `blocked` record at
   0.1 s, so a startup debounce must not swallow NeedsInput.
6. **`tab.dump` carries no agent record** (`TabDumpResult`,
   `crates/roost-ipc/src/messages.rs:478`). The reason rides `Tab` (in
   `tab.list` and snapshots) and `agent_report.changed`.

## Re-capture, 2026-11-08

**Captured on the Pop!_OS dev box for
[agent-integration.md](agent-integration.md)'s Phase 0**, right after
Charlie updated the agents. Same harness and replies as above
(`tools/agent-status/capture.py`), run under `env -i` with a temp `HOME`,
XDG dirs, `TMPDIR` and the agent's home variable under the session
scratchpad, with every updater off and no `ROOST_*`, `CLAUDE*`, `CODEX_*`
or `HERDR_*` variables. grok, Codex and cursor ran logged in from a copy
of the login file only, deleted afterwards. Claude was not re-captured.

### Versions

| Agent | Version | Roost hook source | 7501 `app` |
|---|---|---|---|
| Grok CLI (xAI) | 1.0.77 (50ca579fa996) | `grok` | **`grok`** (was `grok-build`) |
| Codex | codex-cli 0.167.0 | `codex` | `codex` |
| opencode | 1.19.5 (installed and npm `latest`) | `opencode` | none |
| cursor-agent | 2026.11.05-86047df | `cursor` | none |
| crush | 0.97.1 (apt) | none | none |

### What each sends

**Grok CLI 1.0.77.** Probes first (`OSC 7501;?`, about 0.1 s after
start), with or without `TERM_PROGRAM=Roost`. Terminator ST.
- Startup signed out: `blocked`, `kind=auth`, msg "Sign in to Grok".
- Startup signed in with **no hook file anywhere**: `idle`, no trust
  prompt. With any hook file (an isolated `~/.claude/settings.json`,
  `~/.cursor/hooks.json`, or Roost's own `$GROK_HOME/hooks/roost.json`):
  `blocked`, `kind=permission`, msg "Trust this folder?", then `idle`
  after `y`. This explains the 1.0.70 trust-prompt record above.
- A turn: `working` "Waiting for response…", then `working` with
  "Thinking", "Responding" or "Writing command…", then `done` with
  "Turn complete in <n>s."
- A permission prompt: `blocked`, `kind=permission`, msg is the tool
  call's one-line summary. Rejecting it: `working` with the summary, then
  `idle`, no `done`.
- A subagent permission: `blocked`, `kind=permission`, msg "Allow
  <subagent>?". Approving: the root goes `working` "Waiting for
  subagent…", and a **child record** appears (`id=s0/<hex>`, `title` =
  the subagent's name, msg "Working", then "Waiting for response…", then
  `done` "Finished"). When the root reaches `done`, the child gets
  `clear` with the same id. The root record never carried `id` or
  `title`.
- Ctrl-C mid-turn: `idle` within about 60 ms. Esc does **not** interrupt
  (it scrolls back in grok); the turn ends `done`.
- Network failure: `working` with "Connection failed | Retrying (n/15)"
  for over 90 s, with backoff.
- `error`: not captured. grok's embedded docs say a failed turn reports
  its error at once and keeps it until seen; a bogus `-m` model only
  falls back with a warning.
- Exit (Ctrl-C twice, or a kill): `OSC 9;4;0;0`, then `clear`.
- From the embedded docs only (Strings): a `[ui.notifications.program_status]`
  table (`mode`, `query`, `children`), per-session child records with
  several sessions open, at most 63 children, and a "still running"
  count that keeps the record `working` after a turn.

**Codex 0.167.0.** Same states as 0.165.0. Does not probe; terminator ST.
- Startup flash `idle`, `working`, `idle` within about 1 s.
- A turn: `working`, then `done`; the next keystroke sends `idle`.
- An approval (`-s read-only -a on-request`): `blocked`,
  `kind=permission`, **no msg**, although the overlay shows a "Reason:"
  line. Approving goes back to `working`.
- Esc mid-turn: `idle` within 23 ms.
- A failed turn (an unsupported model): `error`, no msg; a keystroke then
  sends `idle`.
- Exit (Ctrl-C): `clear`. New in 0.167.0: the TUI starts a managed
  `codex app-server` daemon that outlives it, and on exit says the task
  continues and can be resumed with `codex resume <id>`. So `clear` means
  the TUI left the tab, not that the agent stopped. Ctrl-Z was not run.

**opencode 1.19.5.** No probe and no record at startup or through a real
turn (the default free model, no login). The only OSCs were 99 and 1337
capability queries, 66, colour queries and titles. No 7501 code in the
binary (every match is CJK tables or SVG numbers; Inferred).

**cursor-agent 2026.11.05.** No probe, no 7501 and no 9;4 at startup or
through a turn. OSC traffic: OSC 0 titles ("Cursor Agent", then one made
from the prompt) and OSC 11 queries; it turns on modes 1004, 2004 and
2031. It sends OSC 777 (Ghostty, Warp), OSC 9 (iTerm2) or OSC 99 (kitty,
VS Code) "Cursor is waiting for you" only for a terminal it recognises by
environment, and only while the window is unfocused: with
`GHOSTTY_RESOURCES_DIR` set and a focus-out, a finished turn sent
`ESC]777;notify;Cursor;Cursor is waiting for you`; with
`TERM_PROGRAM=Roost` it sent nothing.

**crush 0.97.1 (apt).** No probe and no record, on the onboarding screen
or with a turn submitted at the composer. Only the window title and an
OSC 99 query. No `7501` string in the binary. The 2026-10-28 records
above came from the 0.98.2 tarball.

### What this changes

1. **Source → app (rule 1 above):** grok now reports `app=grok`. Roost's
   `app_for_source("grok")` still returns `grok-build`
   (`crates/roost-engine/src/program_status.rs:604-611`), so a grok
   1.0.77 hook claim is dropped as `ForeignApp`, and `app_label` has no
   entry for `grok`, so rows and banners say `grok`. Accept both values
   for the `grok` source and label both "Grok". gx support is being
   dropped, so the gx sentence in rule 1 goes too.
2. **Child records (rule 4 above) are no longer theoretical:** grok 1.0.77
   sends them for subagents, so the "highest-ranked live record" rule
   now runs on real input.
3. **Banners at startup (rule 5 above):** grok's startup trust prompt
   appears only when a hook file exists. Once Roost stops writing
   `roost.json` (and Claude's entries leave `settings.json`), a grok user
   with no hooks of their own starts at `idle`.
4. **crush:** 7501 needs crush 0.98 or newer; the apt build gives Roost
   nothing.
