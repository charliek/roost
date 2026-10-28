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
