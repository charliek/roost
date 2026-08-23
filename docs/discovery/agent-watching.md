# Agent watching (discovery)

Status: **discovery** — not a commitment. Written 2026-08-22 as a
companion to [`host-sessions.md`](host-sessions.md). Host sessions
should not wait on this; this should not wait on SSH.

Roost already hosts agents and routes attention (OSC 133, Claude
hooks, four-axis state, sidebar rollup, notifications). Herdr watches
*many* agents, including ones that never grew a hook API, by reading
the live screen. The question is how to expand what Roost can see
without throwing away the model we already shipped.

---

## Two philosophies

| | Roost today | Herdr |
|---|---|---|
| Default signal | Cooperation: OSC 133 + `tab.agent_report` | Observation: foreground process + screen |
| Claude | Lifecycle hooks via `roostctl claude-hook` | Screen manifest; hooks only for session resume |
| Codex, Cursor, Grok, … | Unrecognized TUI in a tab | TOML manifests against the bottom buffer |
| Blocked / needs input | Hook `notification_type` (strict set) | Visible approval/question chrome (strict) |
| Authority | One derived `tab.state` from four axes | One status authority per pane |
| Where it runs | UI process (`roost-engine`) | Session server |

Roost optimized for **correctness when the agent talks to us**. Herdr
optimized for **coverage when it does not**. Both are right. Expanding
Roost means adding observation as a *fallback writer* into the four
axes, not replacing hooks.

Pinned Roost decisions that still hold (`AGENT_ROADMAP.md`, DL-13):

- Derived state, never a third `set-state` writer fighting the others.
- `source` is an open string (`claude`, later `codex`, `screen:codex`).
- One op: `tab.agent_report`.
- Adapters are pure (`roost-agent`). A second agent is a new module +
  `roostctl <agent> install`.

Herdr’s complementary pins (`docs/next` agents page, `Agents.md`):

- Screen detection is evidence-based: invariant chrome, explicit
  AND/OR, not whole-pane incidental text.
- Snapshot is the **live bottom buffer**, not the user-scrolled
  viewport.
- Transcript/history viewers set `skip_state_update`.
- Blocked is strict: no matching blocker → known agent falls back to
  `idle`, never a guessed `blocked`.
- Full lifecycle hooks, when present, **skip** screen for that pane
  so two authorities do not fight.
- Process identity first, then rules for that agent.

---

## What Herdr actually watches

Stack, in order:

1. **Foreground process** (pgid; optional child-group fallback).
2. **TOML manifests** per agent (`src/detect/manifests/*.toml`):
   regions (`bottom_non_empty_lines(12)`, `osc_title`,
   `after_last_prompt_marker`), `any`/`all`/`not`, priorities,
   `visible_working` / `visible_blocker` / `visible_idle`.
3. **Hot-reload**: bundled + remote herdr.dev updates + local
   `~/.config/herdr/agent-detection/<agent>.toml`.
4. **Lifecycle integrations** when they cover the whole loop (Pi,
   OMP, Kimi, …). Session-only integrations (Claude, Codex resume
   ids) do **not** own status.
5. **Server ops**: `agent wait`, `agent prompt`, `agent read`,
   `agent explain`.

Claude in Herdr is *not* hook-authoritative. The claude.toml rules
match OSC title spinners, “esc to interrupt”, background shells, MCP
tasks, permission chrome. That is why Herdr stays accurate when
Claude’s hooks miss an approval dialog. Roost’s Claude path is the
opposite: hooks are the authority, OSC 133 is the shell fallback.

Herdr currently ships manifests for (among others) Claude, Codex,
Cursor, Copilot, Gemini, Grok, Pi, OpenCode, Droid, Devin, Amp,
Kimi, Kilo, Kiro, Hermes, Qwen, Qoder, Antigravity, Cline, Maki.

---

## What Roost has that we should keep

- Four axes: shell / lifecycle / attention / ownership.
- `roost-agent` Claude adapter (verified against Claude’s binary
  hook schema, including `StopFailure`, `background_tasks`,
  unknown `notification_type`).
- Sidebar agent rows, agent palette, project rollup, desktop
  banners, sticky dots that are not focus-cleared.
- `ROOST_TAB_ID` / `ROOST_SOCKET` in every tab, so a host session
  can keep the same hook path.

L1’s original claim still stands: **a second adapter should be an
afternoon** if that agent has a hook JSON we can map. The gap is
agents with **no** trustworthy lifecycle API. That is most of the
table.

---

## Recommended shape

Do not run a second status field. Observation writes the same
axes:

| Signal | Writes |
|---|---|
| OSC 133 | `shell_state` (already) |
| Hook adapter | `ownership` + `agent_lifecycle` + attention (already) |
| Process identity | `ownership.source` when no live owner (new) |
| Screen manifest | `agent_lifecycle` when no live hook authority (new) |

Authority, copied from Herdr: if a hook adapter currently owns the
tab (`ownership` live and lifecycle ≠ inactive), **do not** also
apply screen rules for that tab. Screen may still raise a
`visible_blocker` as diagnostic later; v1 should not let a regex
override a live Claude session.

`source` values: `claude`, `codex`, `cursor`, … for adapters;
`screen:codex` (or process name) for observation so the sidebar can
tell “Codex told us” from “we inferred Codex.”

### Where it runs

On **`roost-engine`**, because that is where the VT and OSC drain
already live. For a connected host, that means **`roost-session`**,
not iced. The client only displays derived tab state from workspace
events. Superlogical’s rule applies here too: the server owns
authoritative process state; the client owns scroll/select.

Screen text must come from the **server Terminal’s bottom buffer**,
never the iced viewport. Users will scroll host tabs. Herdr already
learned this the hard way.

### What to steal from Herdr’s detector

Steal the **engine**, not necessarily the product:

- Evidence-based TOML: regions, AND/OR, skip-state for transcript
  viewers, strict blocked.
- `explain` output (`roostctl doctor` already exists; extend it
  rather than a new surface).
- Process identity before screen.
- Hot-reload of local overrides. Remote auto-update of manifests
  can wait.

Do not steal in the first cut:

- `agent wait` / `agent prompt` orchestration.
- Native `--resume` of every vendor’s session id (Roost does not
  restore live processes on engine restart today; host sessions
  change that for *detach*, not for `roost-session` crash).
- Running Herdr as a sidecar detector.

License: Herdr is Apache-2.0, so adapting manifest *ideas* or even
TOML rule files with attribution is legally possible. Still treat
their live manifests as a rapidly moving dataset (Claude spinner
shapes change often). Prefer a Roost-owned rule corpus, informed by
Herdr’s structure, over a hard vendor of their tree.

### Adapters vs screen

| Path | Use when |
|---|---|
| `roost-agent` module + `roostctl <agent> install` | The agent has lifecycle hooks we can map (Claude today; Codex/others if/when their hook JSON is stable) |
| Screen + process | The agent is a TUI we can see but not subscribe to |

Ship adapters first where they exist. Screen is how the long tail
gets a dot at all. Both write `tab.agent_report` (or an internal
engine equivalent that goes through the same apply function).

---

## Execution, independent of host sessions

Can start on today’s in-process engine. Host sessions inherit it
when detection lives in `roost-engine`.

1. **Codex adapter spike** (or the next agent that actually has
   hooks). Proves L1’s “afternoon” claim. Sidebar/palette should
   light up with `source = "codex"` and no protocol change.
2. **Process identity** on a tab: foreground command → optional
   ownership label when unowned. Cheap, useful even without
   screen (“this tab is `codex`”).
3. **Screen rule engine** in `roost-engine`: TOML, regions, bottom
   buffer from libghostty, skip-state, strict blocked. Start with
   one agent (Codex or Claude-as-fallback) and `roostctl doctor`
   explain.
4. **Corpus growth**: one agent at a time, live pane evidence, not
   giant fixture screenshots (Herdr’s own guidance: live reads,
   not huge golden TUI dumps).
5. Only later: wait/prompt-style automation, if Roost wants Herdr’s
   orchestration, not just watching.

Do not couple step 3 to Phase 1 of host sessions. If both land in
the engine, connecting a host automatically watches remote agents
because the session process runs the detector.

---

## Open questions

- First screen agent: Codex (huge Roost audience, weak hooks) vs
  Claude-as-fallback (compare to existing hook truth).
- Whether `visible_blocker` may ever override a live hook (Herdr
  allows a stronger visible blocker in some cases). Default no.
- How much of Herdr’s TOML dialect to copy vs a smaller Roost
  schema.
- Whether process identity without a manifest is enough to tint a
  tab (D1) before screen rules exist.

---

## Sources

- Roost: `AGENT_ROADMAP.md`, `crates/roost-agent`,
  `docs/guides/notifications.md`, `docs/guides/claude-code.md`,
  `docs/reference/ipc.md` (`tab.agent_report`).
- Herdr: `docs/next/website/src/content/docs/agents.mdx`,
  `agent-automation.mdx`, `src/detect/manifest.rs`,
  `src/detect/manifests/*.toml`, `Agents.md` (screen detection
  is evidence-based; bottom buffer; not the user viewport).
