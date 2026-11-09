# Agent integration: no writes to agent configs, and a better agent state (discovery)

Status: **discovery**, not a commitment. Written 2026-11-08, rewritten
the same day as the definitive note for this area, then given an
adversarial review the same day (three more live probes, migration
hazards, phase sizes; see [Review](#review-2026-11-08)). Later that day
Charlie answered the first round of key questions; his decisions, the
Codex-without-hooks discovery, the startup prompt design and a
plain-language note on ownership after an agent dies are folded in
(see [Second pass](#second-pass-charlies-answers)). In the evening
Charlie answered the second round (drop gx, Cursor as an unchecked
opt-in, a startup card only when there is something to ask, Claude's
policy handling, no old Claude, the skill in the env plugin), and four
Phase 0 probes ran against the agents he had just updated (see [Third
pass](#third-pass-charlies-second-answers-and-phase-0-probes)). File and
line references are as of main @ 68231b9b. Third-party references are
pinned in [References](#references-and-licenses).

This note merges three research passes (what Roost writes and how to
stop; which delivery routes work, probed live; how to make the agent
state better) after checking each against the code and against each
other. [Corrections](#corrections-to-earlier-notes-and-reports) lists
what did not hold.

Related notes:

- [`agent-program-status.md`](agent-program-status.md) is the **capture
  record**: what each agent sends over OSC 7501, captured 2026-10-28 and
  re-captured 2026-11-08 for grok 1.0.77, Codex 0.167.0, opencode 1.19.5,
  cursor-agent 2026.11.05 and crush 0.97.1.
- [`agent-watching.md`](agent-watching.md) (2026-08-22). This note
  supersedes its "Two philosophies" table, its Codex spike step (shipped)
  and its premise that screen rules come second. Still valid there:
  process identity as a cheap signal, the "server terminal's bottom
  buffer, never the client viewport" rule, and the herdr licensing
  guidance.
- [`backlog-triage-2026-11.md`](backlog-triage-2026-11.md) owns
  scrollback find (#167, its Plan B) and close confirmation (#706, its
  Plan C, which shares `ForegroundLeader` with Phase 3 here). It ranks
  resume on restore as its item 13 and hands it to Phase 7 here, and
  hands #519 and #621 to Phase 3. Its Plan A (v0.1.1 quick wins) ships
  before this track starts. Find is not an agent-state tool and is not
  covered here.

Evidence labels used below:

- **Proven**: run on this box today, in an isolated home.
- **Source**: read in the agent's source at the pinned commit.
- **Docs**: the vendor's published docs.
- **Strings**: read from a closed binary's strings. Not run.
- **Inferred**: reasoned, not observed.

---

## Summary

- **Roost writes into five agents' config directories today, in six
  files.** Three are merges into files the user edits:
  `~/.claude/settings.json`, `~/.codex/hooks.json` plus
  `~/.codex/config.toml`, and `~/.cursor/hooks.json`. Two are files Roost
  owns outright: `$GROK_HOME/hooks/roost.json` and
  `~/.config/opencode/plugins/roost-agent-state.js`. A connecting client
  also makes a remote host write the same files into the host's home.
- **Two agents can take Roost's hooks from the tab environment, with no
  file of theirs touched. Both proven.**
  - Claude Code: `CLAUDE_CODE_PLUGIN_DIRS` pointing at a Roost-owned
    plugin folder. Proven in `-p` and in the interactive TUI, and
    **additive**: a marketplace-installed user plugin, the user's own
    env folder and Roost's appended folder all load and fire, with
    `settings.json` untouched (Proven, 2.1.319). Floor v2.1.280 (Docs).
  - opencode: `OPENCODE_CONFIG_CONTENT` naming a Roost-owned plugin file.
    Proven with a user `opencode.json` present; the user's file was
    byte-identical afterwards (re-proven on 1.19.5).
- **Codex can take hooks per launch on argv only**: `-c` hooks plus one
  inline-table `hooks.state` with hashes Roost computes. Proven in `exec`
  and the TUI, with no trust bypass and a negative control. The user's
  own trusted hooks, in `hooks.json` or in `config.toml`, keep running
  beside it (Proven in review, a0–a4). It needs a shell function to
  reach argv.
- **Grok needs no route: 7501 alone gives it a live, clickable row**
  (grok 1.0.77, Proven captures; the row from code). **cursor's
  `--plugin-dir` is proven but partial**: with a real login it delivers
  session start and end, thought and tool events with no file written,
  but cursor runs `beforeSubmitPrompt`, `afterAgentResponse` and `stop`
  only when `~/.cursor/hooks.json` or a project `.cursor/hooks.json`
  lists them, so a plugin-only cursor has no turn-end signal (Proven,
  2026.11.05).
- **7501 already covers the states Roost shows** for Claude, Codex and
  Grok CLI. grok 1.0.77 also sends child records for subagents. crush and
  pi send less in what was captured: crush showed no `blocked` (and the
  installed apt 0.97.1 sends no 7501 at all), and pi only `idle` and
  `clear` (no turn ran). What only hooks add is identity and detail:
  session id, start source, model and title, the tool on a Codex
  approval, error class, background-task counts.
- **So a default with zero writes is reachable now.** Claude and opencode
  by environment, Codex and Grok on 7501 alone, cursor's file only if
  the user opts in.
- **Decided (Charlie, 2026-11-08): Roost removes its own entries
  automatically once they are no longer needed**, but only what it can
  prove it wrote; everything else is left and named. See
  [Migration](#migration-off-config-writes).
- **Decided (Charlie, 2026-11-08, second round):**
  - **Drop gx support entirely.** Grok moves to 7501 with no Roost file;
    `~/.grok/hooks/roost.json` is retired by the same proven-ownership
    retire, and the gx-specific code, docs and tests are deleted (Phase
    4). No 7501 work in the fork and no gx upstream ask.
  - **Cursor:** file delivery stays an explicit opt-in, **unchecked by
    default**. It is the one agent that still needs a user file.
  - **The startup card appears only when it has something to ask**: a
    file-writing opt-in for an installed agent (today only Cursor).
    Otherwise there is no card; env and 7501 agents are simply on, with
    per-agent off switches in settings, config and `roostctl`. Upgraders
    whose old entries are retired get a one-time toast, not a modal. See
    [The startup prompt](#the-startup-prompt-agent-integration).
  - **Claude:** env plugin via `CLAUDE_CODE_PLUGIN_DIRS`, additive to the
    user's plugins. Policy handling is option A+C: detect
    `disableSideloadFlags` with a cheap probe and skip, plus a doctor line
    and the per-agent off switch. **No support for Claude older than
    2.1.280** and no `--file` fallback; the marketplace or plugin-install
    route can be revisited later if env delivery fails, but is not
    pursued now.
  - **The Roost skill rides in the same env plugin**, which is named
    `roost` so that inside a Roost tab it replaces the marketplace `roost`
    plugin instead of loading twice (Proven).
- **A bug in today's code, found by the grok probe.** grok 1.0.77 reports
  `app=grok`, not `grok-build`. Roost maps the `grok` hook source to
  `grok-build` (`crates/roost-engine/src/program_status.rs:604-611`), so
  with today's `roost.json` installed every grok hook claim is dropped as
  `ForeignApp`, and the row is labelled lower-case `grok` (`app_label`,
  `crates/roost-ipc/src/program_status.rs:175-185`). Two match arms fix
  it; ship the fix before anything else (Inferred from code, high
  confidence; the `app` value is Proven).
- **Codex without hooks already meets Charlie's bar.** A hook-free Codex
  gets an Agents panel row and an agents palette row that update, show
  status and take you to the tab: proven from code, CI tests and a live
  Codex 0.165.0; its 7501 stream is unchanged on 0.167.0 (Proven). A
  hooked Codex already shows 7501's state today, so dropping its hooks is
  almost invisible. See [Codex without hooks](#codex-without-hooks).
  Charlie's second-round answers assume Codex needs no file (key question
  5).
- **A dead agent can leave a stale row or a stuck tab today.** Ending
  ownership when the agent's process is gone fixes it; see [Ownership
  after an agent dies](#ownership-after-an-agent-dies).
- **New risks.**
  - A managed `disableSideloadFlags` policy makes Claude **exit 1 without
    starting** when `CLAUDE_CODE_PLUGIN_DIRS` is set (now **Proven**,
    reproduced rootless on Linux in a user and mount namespace). A
    0.1 s probe, `claude plugin list --json` with the variable set,
    detects it; whether it also catches a server-managed policy is
    Inferred (only once an earlier Claude run has cached that policy).
  - A `roost-session` keeps running across a UI upgrade. If the new UI
    removes the file entries while an older session still serves tabs,
    those tabs get neither file nor env delivery. The retire has to wait
    for that machine's session (see [The retire step](#the-retire-step-integration-version-4)).
  - Claude does **not** dedupe a hook that arrives both from
    `settings.json` and from the env plugin, even with an identical
    command (Proven). Roost raises a banner per report with no dedupe
    (`crates/roost-engine/src/workspace.rs:3384`). The migration window
    needs a small engine fix.
  - Roost's cursor installer writes `hooks.json` under
    `CURSOR_CONFIG_DIR` (`crates/roost-agent-install/src/home.rs:23`),
    but cursor 2026.11.05 reads user hooks only from
    `$HOME/.cursor/hooks.json` (Proven). A user who sets the variable
    gets hooks cursor never runs.
  - Codex 0.167.0 and cursor both leave a detached background process
    that outlives the TUI (Codex's managed app-server daemon, cursor's
    `worker-server`; Proven). The process layer must key presence on the
    TUI process, not on these.
- **Better integration, at the same time.**
  - A process layer: the foreground process names the agent and its exit
    ends stale state. That closes #519 and #621 and meets the 7501 spec's
    "must drop on process exit", which Roost does not do today.
  - Attribution by the caller's pid on the socket, not only by
    `ROOST_TAB_ID`. That addresses #623 and #624 with no wire field.
  - Kind-specific copy ("Needs approval", "Has a question", "Needs
    sign-in"), child rows (#649), done-until-seen, a safer `tab prompt`
    (#516), and resume on restore.
- **Roadmap:** nine phases, ten plans (Phase 3 is two), after a small
  standalone fix for grok's `app=grok`. Env delivery and the retire of
  file entries (now Claude, opencode, Codex and Grok together) come first
  and ship in one release; the process layer can run in parallel. Phase 4
  shrinks to cursor and deleting gx. Phases 1, 2, 3 and 5 are each L or
  more. See [Roadmap](#roadmap).

---

## Goals

Charlie, 2026-11-08: this is the area he most wants to pursue.

1. **Roost stops writing into users' agent configs**, wherever an agent
   allows it. Entries Roost wrote that are no longer needed are removed
   **automatically**, and only where Roost can prove it wrote them; the
   rest are left and named (Charlie, 2026-11-08).
2. **The agent integration gets better at the same time**, not just
   equal. The hooks work fairly well today; the problem is mutating the
   user's config.

Not goals here:

- Screen rules (herdr-style manifests). Last resort; not on the roadmap.
- An Agent Client Protocol (ACP) client. It replaces the agent's TUI,
  which is outside "one terminal per tab". Track only.
- Scrollback find (#167). See `backlog-triage-2026-11.md`.

---

## Today

### The one command every entry runs

Every hook entry runs `installed_command(agent)`
(`crates/roost-agent-install/src/command.rs:105`), integration version 3
(`:63`). It is a `sh -c` that runs `"${ROOST_AGENT_HOOK:-}" agent-hook
<agent>` when that variable is set, prints its answer, and otherwise
drains stdin and prints `{}`.

- **Ownership is byte equality** against `owned_commands` (`:141`): the
  v3 string, v2 (`:117`, bare `$ROOST_AGENT_HOOK`, which grok refuses)
  and v1 (`:131`). `looks_edited` (`:160`) only feeds doctor's "modified
  Roost entry" line. It never removes anything.
- **`ROOST_AGENT_HOOK`** is set per tab by the engine
  (`crates/roost-engine/src/pty.rs:2732`). It names `roostctl`
  (`crates/roost-engine/src/process.rs:255`) or, on a host, the session's
  own binary (`crates/roost-session/src/agent_hook.rs:42`). An inherited
  value is dropped.
- **The verb** is `roostctl agent-hook <agent>`
  (`crates/roost-cli/src/main.rs:2235`). It reads `ROOST_TAB_ID` and
  `ROOST_SOCKET`, runs the pure adapter, sends `tab.agent_report`, and
  answers `{}`.
- **Timeout** is 10 s (`command.rs:73`), lowered to codex's own 3 s cap
  on two events (`crates/roost-agent-install/src/codex.rs:57`).

### What is written, per agent

| Agent | File (env override) | What is written | Kind of edit |
|---|---|---|---|
| Claude Code | `settings.json` in `~/.claude` (`CLAUDE_CONFIG_DIR`) | One group on each of 11 events (`CLAUDE_HOOK_EVENTS`, `crates/roost-agent/src/claude.rs:80`) | Merge. JSON keeps meaning, key order and number tokens, not bytes (`crates/roost-agent-install/src/lib.rs:19-42`). A group holding a user's `matcher` survives removal |
| Codex | `hooks.json` and `config.toml` in `~/.codex` (`CODEX_HOME`) | 8 events (`crates/roost-agent/src/codex.rs:63`). In `config.toml`: `[features] hooks = true` and one `[hooks.state."<abs hooks.json>:<event>:<i>:<j>"] trusted_hash` per handler (`wire_config`, `roost-agent-install/src/codex.rs:477`) | Merge. TOML via `toml_edit`, byte-preserving outside Roost's tables. The trust hash is a port of Codex's formula (`codex_hash.rs`). Keys are path- and index-based (`key_source`, `:82`) |
| Grok CLI and gx | `hooks/roost.json` in `$GROK_HOME` (default `~/.grok`) | 11 events (`crates/roost-agent/src/grok.rs:91`) | Whole file Roost-owned, and only if `is_ours` holds (`roost-agent-install/src/grok.rs:70`) |
| cursor-agent | `hooks.json` in `~/.cursor` (`CURSOR_CONFIG_DIR`) | 8 events (`crates/roost-agent/src/cursor.rs:82`), plus `"version": 1` if absent | Merge |
| opencode | `plugins/roost-agent-state.js` in `~/.config/opencode` (`OPENCODE_CONFIG_DIR`) | The shipped plugin (`crates/roost-agent/assets/opencode/roost-agent-state.js`) under the header `// managed by roost — agent hooks, integration version 3` | Roost-owned if the whole first line is the marker plus an integer (`roost-agent-install/src/opencode.rs:29,55`). Local edits are overwritten |

"Present" means the agent's config directory exists. Roost never creates
an agent's directory (`roost-agent-install/src/home.rs:153`).

### Roost's own bookkeeping

- **Consent key:** `agent-hooks` in `config.conf`
  (`crates/roost-ui-model/src/config.rs:87`, enum at `:257`). A list,
  `off`, or absent meaning "ask". Unknown names are kept (#486's fix).
- **State record:** `~/.config/roost/agent-hooks.json`. The path is
  hard-coded under `$HOME/.config`; it ignores `XDG_CONFIG_HOME` and
  `ROOST_CONFIG` (`home.rs:159-166`). It holds, per agent: the integration
  version, files touched, files Roost **created** (the only ones it may
  delete), codex's `[features] hooks` pre-image, `noticed`, and who wired
  it.
- **Lock:** one `ConfigLock` beside `config.conf` (`home.rs:184`).
- **Test fence:** under `ROOST_TEST_MODE=1` every write is refused unless
  `ROOST_AGENT_HOOKS_FORCE=1` (`crates/roost-agent-install/src/plan.rs:100-103`).
- **Writes are careful already:** digest re-check before rename,
  symlink-preserving atomic writes, rollback across codex's two files.
  What a bulk migration lacks: **no persistent backups** (rollback is
  in-memory within one apply) and **no "retired" state** (the tombstone
  #485 asks for).

### Who writes, and when

| Entry point | What runs | Removes? |
|---|---|---|
| UI startup, once per process, off the UI thread (`crates/roost-iced/src/app.rs:4454` → `app/agent_hooks.rs:156`) | `ensure`: reads the key under the lock; `ask` and `off` write nothing (`roost-agent-install/src/ensure.rs:328`) | Never (plan 064 §3.2) |
| First-run consent card (key absent, an agent present) and its preferences mode | Apply → `agent.set_hooks` → `set_hooks` (`ensure.rs:399`) | Yes |
| Apply, pushed to every connected non-localhost host | `session.set_agent_hooks` | Never (raise only) |
| Host connect (`app/agent_hooks.rs:686,709`) | `session.set_agent_hooks` with the client's list | Never |
| Host side (`crates/roost-engine/src/ipc.rs:3075` → `crates/roost-session/src/agent_hooks.rs`) | `raise` (`ensure.rs:371`) on the **host's** `$HOME`: unions the host key and wires | Never |
| `roostctl agent ensure` / `set` / `install` / `uninstall` | `reconcile` / `set_hooks` / `install` (`ensure.rs:537`) / `uninstall` (`:563`) | `ensure`, `set`, `uninstall`: yes |
| `roostctl agent status`, doctor | read-only | no |

Remote hosts, in short:

- `roost-session` never runs `ensure` at its own start. A host's files are
  written only when a client raises it.
- A raise widens the host key even when the host said `off` or never
  answered (plan 064 §3.3).
- The localhost session is never raised: it reads the same `config.conf`
  as the UI (`crates/roost-iced/src/host_conn.rs:2508-2512`).
- The mobile client never raises.

### What the hooks feed

Adapters are pure (`crates/roost-agent/src/`). They emit
`tab.agent_report` on the DL-13 axes, with ownership per DL-14.

| Hook-only signal | Source | Consumer today |
|---|---|---|
| `session_id` | every adapter | DL-14 ownership and release, nested-claim refusal, doctor's owning check |
| `SessionStart.source` | Claude, Codex, grok | `detail` only. #623 needs it as a contract |
| session title | Claude only (Codex's adapter sends `model` and `permission_mode`, no title: `crates/roost-agent/src/codex.rs:136-146`) | the row's name line, else the tab title |
| model, permission mode, cursor version | adapter metadata | **no UI surface**; only `roostctl doctor` prints them (`crates/roost-cli/src/doctor.rs:1613` allowlist) |
| PermissionRequest `tool_name` | Claude, Codex | "Needs permission to use `<tool>`" banner body |
| Notification type | Claude, grok | banner title; `waiting` |
| Stop `background_tasks`, `session_crons` | Claude | keeps the tab working; "Working · N bg tasks" |
| StopFailure / session.error class | Claude, opencode | Failed banner text |
| `agent_id` inside a subagent | Claude | subagent events cannot move the owner's state |
| `server_url` | opencode plugin's loopback listener | driving an opencode session |
| `gx.remote` | grok/gx | doctor display. Goes when gx support is dropped (Phase 4) |

Agents panel and palette rows already appear for 7501-only tabs
(`crates/roost-ui-model/src/agent_palette.rs:221-244`). `roostctl wait`
and `tab prompt` read the composed `tab.state`, so both already work from
7501 alone.

### What users carry today

- Three of the six files are merges into files the user edits. JSON
  reformatting on first write cannot be undone (`lib.rs:19-42`).
- Every event on every machine carrying the entries spawns `sh` and
  `cat`, even with no Roost running.
- **Cross-agent execution.** grok-build's `VendorCompat` defaults hooks on
  for Claude and cursor (Source:
  `crates/codegen/xai-grok-config/src/compat.rs:297-306`); its discovery
  reads `~/.claude/settings.json` and `~/.cursor/hooks.json`
  (`crates/codegen/xai-grok-hooks/src/discovery.rs:342-400`). cursor
  loads `~/.claude/settings.json` hooks (`claudeUserHooks`; Strings). So
  a grok event in a Roost tab runs Roost's grok and Claude entries. The
  adapters reject foreign payloads (Claude's rejects grok's camelCase
  `hookEventName`, `crates/roost-agent/src/claude.rs:132`), but the
  processes are spent. grok 1.0.77, live (Proven): it fires hooks from
  an isolated `~/.claude/settings.json` (SessionStart, UserPromptSubmit,
  Stop); it does **not** run Roost's flat `~/.cursor/hooks.json` entries
  (only matcher-group entries); and **any** hook file, Roost's own
  `roost.json` included, costs a "Trust this folder?" prompt in each new
  folder. With no hook file, grok starts with no trust prompt. Moving
  Claude to the env plugin ends the cross-fire: grok's binary has no
  `CLAUDE_CODE_PLUGIN*` string (Strings).
- Codex trust keys break when entries are reordered or `CODEX_HOME`
  moves. Each break is one review dialog.
- #485: a hand-deleted entry comes back on the next `ensure`.
- #492: a raise does not forward a name the client cannot wire.
- #553 item 2: quit can cut an ensure between two file updates.

### What the docs promise

These change when the migration ships (Phase 2):

- `docs/guides/agents.md`: the lede ("wiring one hook entry per lifecycle
  event into each agent's own configuration file"), the "Config file Roost
  writes into" column, consent, opt-out, codex trust, the guarantee, and
  the remote-hosts section.
- `docs/getting-started/first-run.md` (the consent card),
  `docs/reference/config.md` (`agent-hooks`), `docs/reference/cli.md`
  (`agent` verbs), `docs/reference/ipc.md` (`session.set_agent_hooks`),
  `docs/guides/host-sessions.md`, `docs/guides/claude-code.md`.
- `docs/development/vision.md`, "Agent awareness": "agent hooks that
  `roostctl agent install` writes into each agent's config, with
  consent".

Already stale, independent of the migration (found by the first pass; fix
in Phase 2):

- `README.md` still says "copy-paste `settings.json`".
- `skills/roost/SKILL.md:101` already exempts "agents that report
  directly", so it is not wrong. Its "no hooks installed" wording changes
  with Phase 2.
- `crates/roost-ipc/src/messages.rs` around `:4810` still says no server
  ships "until plan 064 C6 (iced) and C8 (Mac)".
- `app/agent_hooks_dialog.rs`, `ensure.rs` and `acceptance.rs` still
  mention the Mac sheet or the Mac app.

---

## Per-agent signal and delivery matrix

### Delivery that writes nothing into the agent's config

All probes ran with `env -i`, a fresh `HOME`, the XDG trio, `CODEX_HOME`,
`GROK_HOME` and `TMPDIR` under the scratchpad, and no `ROOST_*`,
`CLAUDE*`, `CODEX_*` or `HERDR_*` variables. In the first passes no agent
was logged in; Claude and Codex ran with fake API keys. Claude, Codex,
opencode and pi fire their start and prompt hooks before the auth check,
so delivery is proven for those four. cursor checks auth first. The
2026-11-08 evening probes copied **only** the login file into the
isolated home for cursor (`~/.config/cursor/auth.json`), grok
(`~/.grok/auth.json`) and Codex (`~/.codex/auth.json`), confirmed each copy
was never refreshed, and deleted them; Claude still ran with a fake key.
Before-and-after sha256 of the real config dirs were identical in every
probe.

| Agent (installed) | Route | Status | Floor | Caveats |
|---|---|---|---|---|
| **Claude Code** 2.1.319 | `CLAUDE_CODE_PLUGIN_DIRS=<roost dir>` in the tab env. The dir holds `.claude-plugin/plugin.json`, `hooks/hooks.json` and (decided) `skills/roost/SKILL.md` | **Proven**, `-p` (c1) and interactive TUI in a PTY (c2): SessionStart, UserPromptSubmit, SessionEnd fired (StopFailure only sometimes; see caveat 11). No `settings.json` created. **Additive** (p0 a): a marketplace-installed `userplug@localmkt`, the user's own env folder and Roost's appended folder all fired, three runs; `settings.json` and `installed_plugins.json` byte-identical | **v2.1.280** (Docs, env-vars page; CHANGELOG never names it). Decided: no support below it | See the Claude caveats below |
| | Second route: `--settings '<inline JSON>'` on argv | **Proven** (c6) | | Needs a shell function. **Not blocked by `disableSideloadFlags`** (Proven under a reproduced policy, p0 c). Using it there would knowingly get around an org's sideload ban, so it is **not** a fallback |
| **opencode** 1.19.5 (installed and npm `latest`) | `OPENCODE_CONFIG_CONTENT='{"plugin":["file:///<roost>.js"]}'` | **Proven** (o1, re-proven on 1.19.5 in a real turn). With a user `opencode.json` declaring its own plugin, both plugins got the same events with the same counts, and the user file was byte-identical (`cmp`) | v1.x (Source: `config.ts:482-495` at v1.19.5) | `plugin` lists accumulate across sources and dedupe by identity; other keys deep-merge, env last. Send only `plugin`. Merge into a user value (one variable). The legacy plugin file plus the env plugin both run (o2). opencode still writes its own `package.json` and `node_modules` in its config dir. The first launch with a plugin is blank for over 4 s while it installs dependencies |
| **Codex** 0.165.0 (argv route); 0.167.0 installed | argv only: `-c 'hooks.<Event>=[{hooks=[{type="command",command=…,timeout=N}]}]'` per event, plus **one** `-c 'hooks.state={"/<session-flags>/config.toml:<label>:<g>:<h>"={trusted_hash="sha256:…"}, …}'` | **Proven** with no bypass flag, in `exec` (x2, x7) and the TUI (x5). Negative control (x3): a wrong SessionStart hash silently skipped SessionStart while UserPromptSubmit still ran. `features.hooks` not needed (x7). Codex wrote no hooks or state to `config.toml` | Trust formula is version-coupled; HEAD `4e4767ffa7` still matches Roost's port | The dotted-key form of `hooks.state` is "ignored" (x1); only the inline table works. The session-flags key is machine-independent, so the hashes are per-integration-version constants. If a hash drifts, the TUI shows "Hooks need review" on every launch (x6), and "Trust all" writes `hooks.state` into the user's `config.toml` (Source: `tui/src/hooks_rpc.rs:64-95`). No env route: `CODEX_HOME` also moves auth and history |
| **cursor-agent** 2026.11.05-86047df (logged in) | argv only: `--plugin-dir <dir>` (`.cursor-plugin/` or `.claude-plugin/` manifest, `hooks/hooks.json` in cursor's or Claude's nested format) | **Proven, but partial** (p0 cursor, real login). Loads in `-p` and the TUI with no setting and no approval. Fired: `sessionStart`, `afterAgentThought`, tool events (`preToolUse`, `beforeShellExecution`, `postToolUse`; seen in `-p`), `sessionEnd`. **Never from a plugin:** `beforeSubmitPrompt`, `afterAgentResponse`, `stop`. The TUI runs those three only when `$HOME/.cursor/hooks.json` or `<workspace>/.cursor/hooks.json` lists the same event (source gate checks only user and project hooks); `-p` never runs them. Payloads carry `cursor_version` and `session_id`; hooks inherit the tab env. Writes nothing to `~/.cursor` | n/a | No env route (`--plugin-dir` has no env binding). `enableUserLocalPlugins` is a runtime capability, true when signed in, not a setting. A shell function can add the flag (both `agent` and `cursor-agent` names point at one launcher), giving presence and working state but **no turn end**. User hooks are read only from `$HOME/.cursor/hooks.json`, never from `CURSOR_CONFIG_DIR` or `$XDG_CONFIG_HOME/cursor` (Proven), which Roost's installer gets wrong. No 7501, no 9;4, no blocked signal |
| **Grok CLI** 1.0.77 | **Not needed.** 7501 alone gives a live row (decided: no Roost file). For the record: `--plugin-dir` exists only on `grok agent`, not the TUI (g1 on 1.0.70, Proven negative) | Decided: 7501 only | n/a | Grok dedupes same-content hooks across sources (first pass; not re-run). Any hook file costs a per-folder trust prompt (Proven, 1.0.77) |
| **gx** 1.0.45+gx.17 | **Dropped** (Charlie, 2026-11-08) | n/a | n/a | No 7501. Roost removes its gx-specific code in Phase 4 |
| **pi** 1.5.0 (`@earendil-works/pi-coding-agent`, via npx; not on PATH) | argv only: `-e <ext.ts>` | **Proven** (p1): `session_start`, `input`, `session_shutdown` | n/a | Roost has no pi adapter. 7501 covers it |
| **crush** 0.97.1 installed (0.98.2 tarball captured 2026-10-28) | **None.** `CRUSH_GLOBAL_CONFIG` relocates the whole config | Inferred (help, docs) | n/a | Has a `hooks` config. FSL-1.1-MIT: docs and design only. 7501 only, and the apt 0.97.1 sends **no** 7501 (Proven); 7501 needs crush 0.98 or newer (Inferred) |

**Claude caveats** (all from c3–c7 unless marked):

1. **Duplicates.** With the user's `settings.json` still holding Roost's
   hooks, every event fired once from each source (c3). Identical
   command strings are not deduped across sources (c3b).
2. **`disableSideloadFlags`.** A managed setting that makes Claude
   "exit `1` without starting" when the variable names a plugin folder
   (Docs: plugins CLI reference and settings reference, "Requires Claude
   Code v2.1.280 or later"). **Now Proven** (p0 c, 2.1.319), rootless:
   `unshare -rm` with a tmpfs overlay on `/etc` holding
   `/etc/claude-code/managed-settings.json` = `{"disableSideloadFlags":true}`
   (also as a `managed-settings.d/` drop-in, and as uid 1000 in a nested
   namespace; the host `/etc` untouched). `-p`, the interactive TUI and
   `claude plugin list` all exit 1 before any hook, with stderr
   "CLAUDE_CODE_PLUGIN_DIRS is disabled by your organization's managed
   settings (disableSideloadFlags). … Unset CLAUDE_CODE_PLUGIN_DIRS …".
   Any absolute path trips it, even one that does not exist. Controls:
   policy `{}` with the variable runs; policy on without the variable
   runs. `CLAUDE_CODE_MANAGED_SETTINGS_PATH` and
   `CLAUDE_CODE_REMOTE_SETTINGS_PATH` are compiled out of release builds
   (Source), which is why c4 failed. Where policy comes from (Source):
   `/etc/claude-code/` on Linux, `/Library/Application Support/ClaudeCode/`
   on macOS, macOS MDM plists under `/Library/Managed Preferences/`
   (root-owned), and **server-managed settings**, cached at
   `$CLAUDE_CONFIG_DIR/remote-settings.json` with a signature. Detection
   is caveat 10.
3. **`disableAllHooks: true`** in user settings silences the plugin's
   hooks too (c5). `allowManagedHooksOnly` blocks them, but it blocks
   today's `settings.json` hooks equally.
4. **A settings-file `env` block that sets the variable is honoured**
   (c7). This refutes the first pass, which said Claude strips it.
5. The value is split on `:`. Only absolute or `~` paths are kept. A
   missing directory is tolerated silently.
6. Claude writes its own state for an env plugin:
   `plugins/data/<name>-inline/` and a `pluginUsage` counter in
   `.claude.json`. Those are Claude's writes, not Roost's.
7. The variable is inherited by children (`CLAUDE_PLUGIN_ROOT` too). A
   nested `claude` loads the plugin, as it loads `settings.json` today.
8. **A value with two folders loads both** (Proven in review, b1:
   interactive TUI, a user plugin first and Roost's second, both fired
   all four events, no `settings.json` created). So appending to a
   user's own value works. **And it is additive to installed plugins**
   (p0 a): a marketplace-installed user plugin kept firing beside both
   env folders.
9. **Two folders whose plugins share a name load only the first**
   (Proven in review, b2: only the first folder's hooks fired). Keep the
   plugin's name constant across versions, so a stale inherited Roost
   folder can never double-fire, and strip inherited Roost folders before
   appending, because the **first** one wins: a stale older copy earlier
   in the list would shadow the current one. **The name is `roost`**
   (decided with the skill, p0 b): an env plugin named like an installed
   marketplace plugin **replaces** it for that session (debug log:
   `Plugin "roost" from --plugin-dir overrides installed version`). With
   the marketplace `roost@roost` installed (with hooks in a test copy),
   only the env copy's hooks fired and its SKILL.md text was the one
   used; one `roost:roost` skill, no duplicate (Proven). `claude plugin
   list` still shows both `roost@roost` (enabled) and `roost@inline`, so
   doctor must not read that pair as duplicate hooks. Only an enabled env
   copy shadows (Source). If managed settings **lock** a plugin named
   `roost`, the env copy is dropped with a warning and Claude runs on
   (Source, not run).
10. **Detecting the sideload policy** (p0 c, Proven): `CLAUDE_CODE_PLUGIN_DIRS=<roost
    dir> claude plugin list --json </dev/null`. No policy: rc 0 and JSON
    on stdout; policy: rc 1, empty stdout, `disableSideloadFlags` on
    stderr; 0.09–0.15 s. It wrote nothing on an existing home (a brand-new
    home gets Claude's own `.claude.json`). `claude --version` and
    `--help` do **not** detect it (they exit before the check). The check
    reads the whole policy layer, so run as the user with the real
    environment it covers files and MDM. A **server-managed** policy is
    caught only once an earlier Claude run has cached it (Inferred: no
    eligible org account to reproduce; the override variables are
    compiled out). The existence of `$CLAUDE_CONFIG_DIR/remote-settings.json`
    is a cheap doctor hint, not proof. `CLAUDE_CODE_RESTRICT_PERSONAL_CONFIG`
    strips the variable when a user `settings.json` `env` block sets it,
    not when it comes from the process environment (Source).
11. **StopFailure is unreliable when `-p` ends in an auth error** (p0 a):
    it fired for some plugins in some runs and none in others, likely
    racing the exit (Inferred). No test may pass on it.

**Codex beside the user's own hooks** (Proven in review, `rev/a0`–`a4`,
codex-cli 0.165.0, fake API key, no bypass flag). The user's own trusted
hooks, in `hooks.json` (a0) or as `[[hooks.SessionStart]]` in
`config.toml` (a3), fired alone. With Roost's `-c` hooks and one inline
`hooks.state` added (a1, a4), both the user's and Roost's fired, so the
`-c` table merges into the user's `hooks.state` and does not replace it.
Negative control (a2): with the user's trust entries removed, only
Roost's hooks fired. Codex wrote nothing to `config.toml` in any run.

**A gap the probes do not cover (Inferred).** Roost sets the tab
environment before the shell starts (`pty.rs:2691`). A user's rc file
that exports `CLAUDE_CODE_PLUGIN_DIRS` or `OPENCODE_CONFIG_CONTENT`
replaces Roost's value. Roost's bash and zsh integration runs after the
rc and can re-append. Tabs started with a command, fish, and Apple's
`/bin/bash` 3.2 get no integration file
(`bash_autobootstrap`, `pty.rs:2162`), so they keep only the spawn-time
value.

**A second gap: the server's own environment (Inferred).** Under DL-32 a
tab's environment starts from the serving process's own environment. A
`roost-session` or a dev UI started from inside a Roost tab inherits that
tab's `CLAUDE_CODE_PLUGIN_DIRS` and `OPENCODE_CONFIG_CONTENT`, which name
another process's Roost folder. A plain append would give each tab two
Roost entries. The engine already drops an inherited `ROOST_AGENT_HOOK`
for this reason; the merge must likewise strip inherited Roost entries
(by a path marker) before appending its own.

### Signals per agent

"Hooks" means Roost's adapter as shipped. 7501 rows come from
`agent-program-status.md` (2026-10-28, plus the 2026-11-08 re-captures of
grok, Codex, opencode, cursor and crush).

| Agent | Roost hooks | OSC 7501 | OSC 9 / 777 / BEL | OSC 9;4 | Process |
|---|---|---|---|---|---|
| **Claude Code** (2.1.312 captured; 2.1.319 startup re-checked) | 11 events; no interrupt hook (#622) | Probes; silent without a reply. idle, working (activity msg), blocked `permission` with "approve <Tool>: <cmd>", done, clear. Esc gives idle in about 0.1 s. No `id`, `title` or children seen in 2.1.312 | `auto` channel matches no terminal under `TERM_PROGRAM=Roost` (Strings) | `terminalProgressBarEnabled` setting exists (Strings); not observed under Roost | `claude` |
| **Codex** 0.167.0 (0.165.0 captured; same states) | 8 events incl. Interrupt | No probe. idle, working, done, blocked `permission` with **no msg**, error with no msg, Esc gives idle, clear; a startup flash | BEL under Roost (Source: `notifications/mod.rs:21-33`) | | `codex`, plus (0.167.0) a detached `codex app-server … --managed-daemon` that outlives the TUI |
| **Grok CLI** 1.0.77 (1.0.70 captured earlier) | 11 events, no PermissionRequest. Decided: retired | Probes. **`app=grok`** (was `grok-build`). idle at start (a trust prompt only when a hook file exists), working with msg, blocked `permission` with the tool's summary, blocked `kind=auth` signed out, done with a summary, **child records** for subagents (`id`, `title`; cleared when the root is done), Ctrl-C gives idle, clear on exit. A rejected prompt goes to idle with no done; Esc does not interrupt | | herdr's grok manifest matches `4;1;-1`; 9;4;0 on exit | `grok` |
| **gx** 1.0.45+gx.17 | shares grok's file. **Dropped** | **none** | | emits 9;4 | `gx` |
| **cursor-agent** 2026.11.05 | 8 events; no blocked signal | **none** (re-checked: only OSC 0 titles and OSC 11 queries) | OSC 777 / 9 / 99 "waiting for you" only for terminals it recognises by env (`GHOSTTY_RESOURCES_DIR`, `ITERM_SESSION_ID`, …) and only while unfocused; **nothing under `TERM_PROGRAM=Roost`** (Proven) | none | `cursor-agent`, plus a detached `node … worker-server` that outlives the session |
| **opencode** 1.19.5 | plugin, 10 bus events incl. permission and question | **none** in any npm release (re-captured on 1.19.5 through a real turn: no probe, no record). Source has a root record plus a child per pending session from `d0a2ce32de` (2026-10-13); first tag v2.2.6 (2026-11-06); npm `latest` is 1.19.5 | | | `opencode` (a node/bun runtime underneath) |
| **crush** 0.98.2 captured; 0.97.1 installed | none | 0.98.2: no probe. idle, working, error. **0.97.1 (apt): none** | | | `crush` |
| **pi** 1.5.0 | none | Probes. idle, clear | | | `pi` (node) |

**Claude 2.1.319's 7501 may be much richer (Strings, unconfirmed).** The
third pass read child records for background tasks, blocked `kind` by
blocker (`question` for "input needed", `permission` for permission,
sandbox, worker and goal prompts), `blocked kind=auth`, `error` with a
message, `done` with a detail, and a `progress` value. A spot check of the
extract found the blocker labels ("sandbox request", "goal proposal") but
not the report builder itself. Phase 0 must capture this live before
Phase 5 designs rows around it.

**What 7501 gives that hooks do not:** interrupts, exits (`clear`),
startup trust and sign-in prompts, no install, and it rides plain ssh
from a Roost tab to a machine with no Roost. **What it lacks:** a session
id (`app` is only a label), model, title, start source, Codex approval
detail, error class. Inside tmux, Claude's probe is answered by tmux, so
Claude stays silent (Inferred).

**Codex has the detail but does not send it.** Codex HEAD computes an
activity and approval detail and writes it only to iTerm2's OSC 21337.
Its 7501 writer emits `state`, `app` and `kind`, never `msg` (Source:
`codex-rs/tui/src/terminal_program_status.rs:149,261-270`). That is the
best upstream ask on the list.

**More Codex 7501 facts** (second pass; Source at HEAD `4e4767ffa7`,
live on 0.165.0):

- After a turn Codex holds `done` (or `error`) **until a key, paste or
  click in Codex**, then sends `idle` (`acknowledge_program_status`,
  `tui/src/chatwidget/status_surfaces.rs:341`, called from
  `tui/src/app.rs:926`). That is Codex's own "done until seen".
- Blocked takes the approval overlay's kind: exec and patch approvals
  are `permission`; MCP elicitation and `request_user_input` are
  `question`; app-link auth is `auth`.
- **Nothing before the composer** (Proven): the sign-in, trust and update
  screens send no 7501.
- Codex starts hook commands in a **new session**
  (`ProcessMode::NewSession`, `hooks/src/engine/command_runner.rs:473`),
  and 0.165+ runs threads in a detached managed app-server daemon (seen
  live). See [Codex without hooks](#codex-without-hooks) for why that
  matters to Phase 3b.
- **0.167.0 (Proven):** launching the TUI starts `codex app-server
  --listen unix:// --managed-daemon`, a `pid-update-loop` and
  `codex-code-mode-host`, installs the daemon under `CODEX_HOME`, and
  these outlive the TUI. On exit Codex says "Disconnected from this task.
  Any running work continues. To reconnect, run: codex resume <id>". So
  `clear` means "the TUI left this tab", not "the agent stopped". Phase
  3a keys Codex's presence on the TUI process; Phase 7 could use the
  printed `codex resume <id>`. Whether a turn really keeps running after
  Ctrl-C mid-turn was not tested. Isolated probes must kill the daemons
  they start.

---

## How others do it

**herdr** (Apache-2.0, `ee5bfb5`).

- Kept states are idle, working, blocked and unknown. "Done" is idle the
  user has not seen.
- Inputs in order: process identity (foreground job, runtime wrappers
  unwrapped to the script, `HERDR_AGENT` override); integration reports
  (full lifecycle for pi, opencode and a few others; session identity
  only for Claude and Codex since v0.6.7 and v0.9.5); screen manifests.
- One arbitration function (`recompute_effective_state`,
  `src/terminal/state.rs:2404`). No TTL. Process exit clears a matching
  hook authority, and a late report from the dead session cannot retake
  it. An agent that exits without releasing is cleared when the shell is
  idle again (`clear_self_reported_agent`, `:1887`). Ctrl-Z keeps
  identity.
- A same-agent report replaces a live session only for start sources
  `clear`, `resume` and `compact`, never `startup`
  (`session_report_allows_session_replacement`, `:1385`). This is the
  rule #623 cites.
- `agent prompt --wait` refuses a blocked agent. Resume argv is reported
  by integrations and re-run after a server restart
  (`src/agent_resume.rs`).
- It still edits `~/.claude/settings.json` when asked, mainly for resume.
  It does not read 7501.

**cmux** (GPL-3.0-or-later; design level only, from its docs).

- 7501 records are terminal state. `done`/`error` stay until the pane is
  focused; `working`/`blocked` drop at a prompt and at process exit.
- Claude hooks are passed per launch by a `claude` wrapper on PATH; a
  settings install is the fallback. Codex is still installed into
  `~/.codex`, with a wrapper adding hooks per run.
- Hook attribution: explicit flags, then the environment, then the
  caller's controlling tty, then the process tree.

**Ghostty** (MIT). libghostty-vt parses 7501 and hands records to the
embedder. Its header says to keep `done`/`error` "until the user has seen
them" and to evict the oldest record over the cap
(`include/ghostty/vt/terminal.h:1400-1430` at `eb895f612`). Ghostty's own
app does not implement 7501. The semantic-prompt event carries the exit
code and command line but no OSC 133 `aid`.

**zellij** (MIT, `43c8710`, unreleased). One attention tracker per pane
over 7501, prompt marks with exit codes, bells, alternate-screen switches
and output bursts, plus last-visible time
(`zellij-server/src/attention/`).

**OSC 7501 spec** rev 0.4 (2026-10-10). No heartbeat. On process exit or a
new prompt a terminal **must** drop `working`/`blocked`. `done`/`error`
survive both. A cap of 256 records, evicting the least recently updated.
Unknown keys are ignored. No `session` key.

**ACP** (Apache-2.0, `7441d10`). JSON-RPC between an editor and an agent
it launches: permission option kinds, tool calls, plans, session
resume. opencode and grok-build ship it. It makes Roost a chat client
beside its terminals. Track it; borrow its vocabulary for field names.

The pattern: nobody likes editing agent configs; the mature tools have
cut hooks back to identity; the one that keeps rich Claude hooks delivers
them on argv. Nobody but cmux and Roost reads 7501 as state.

---

## Target model

### End state per agent

| Agent | Delivery | Roost writes into the agent's config |
|---|---|---|
| Claude Code (2.1.280 or newer; older is unsupported) | Roost-owned plugin folder named `roost` (hooks plus the Roost skill) via `CLAUDE_CODE_PLUGIN_DIRS` on every tab (local, local session, host session), plus 7501. Skipped where the sideload probe finds the policy | Nothing |
| opencode | Roost-owned plugin file via `OPENCODE_CONFIG_CONTENT`, merged with a user value. Once a 7501 release reaches npm `latest`, the plugin keeps only identity (`session_id`, `server_url`) | Nothing |
| Codex | **Recommended, and assumed by Charlie's card answer:** 7501 alone now (meets Charlie's bar today; see [Codex without hooks](#codex-without-hooks)), plus the process layer once Phase 3a lands. Option, not recommended: a `codex()` shell function passing `-c` hooks and `hooks.state` | Nothing |
| Grok CLI | **Decided:** 7501 plus the process layer. `hooks/roost.json` retired | Nothing |
| gx | **Dropped** (decided) | Nothing |
| cursor-agent | **Decided:** file delivery is an explicit opt-in, unchecked by default; without it, process identity gives presence and exit. Option: a `cursor-agent()` shell function adding `--plugin-dir` gives presence and working state with no write, but no turn end (Proven partial) | `~/.cursor/hooks.json` only if opted in |
| pi, crush | 7501 plus the process layer | Nothing |

### Inputs

| | Input | Gives | Reaches a host-session tab? | Reaches plain ssh? |
|---|---|---|---|---|
| P | **Process layer** (new): foreground leader identity (pid, start time, agent by argv), shell in front, alive or stopped, exit code from `133;D;<code>` | presence, exit, suspension | yes (runs in `roost-session`) | sees only `ssh` |
| R | **Records** (7501) | state, kind, msg, title, progress, children | yes | yes |
| H | **Hooks**, env-delivered | session id, start source, model, title, tool name, error class, background tasks | yes (env set by `roost-session`) | no |
| S | **Shell marks** (OSC 133) | prompt and command boundaries, exit code (new) | yes | yes |
| N | **Hints**: OSC 9;4 (new), bell, OSC 9/777/99 | working hint, attention | yes | yes |

### Precedence for the lifecycle the tab shows

Highest wins. Each rule names its end condition. No timers.

1. **A live record** (`working`/`blocked`) whose stamped process is still
   alive. As DL-29 today, plus process exit as an end.
2. **A root `idle`/`done`/`error`** that no prompt mark or process exit
   has followed. As today, plus process exit.
3. **The hook lifecycle**, while the owner's stamped process is alive.
4. **A kept root `done`/`error`** after a prompt or exit, when no hook
   drives the tab, until it is seen or the next `133;C`. This is today's
   `DecidedBy::KeptRecord` branch of `compose`
   (`crates/roost-engine/src/program_status.rs:474`); without it a
   finished turn would stop showing at the first prompt.
5. **OSC 9;4 state 1 or 3** from the current foreground process: Working.
   A 9;4 error does not project Failed. 9;4 state 0 ends the hint.
6. **Process identity alone**: lifecycle Inactive, with an agent row
   "<Agent> · running". It never guesses Working or Waiting (herdr's
   strict-blocked rule).
7. **The shell axis**: `C` is Working, a prompt is Inactive.

`Tab.state` stays the projection of this lifecycle (`none`, `running`,
`needs_input`, `idle`; `crates/roost-ipc/src/messages.rs:38`).
`decided_by` gains `process` and `hint`. An older client reads the whole
summary as absent, which its lenient decode already allows
(`lenient_program_status`, `messages.rs:3641`).

### Field fusion (text)

State comes from the winner. Each display field comes from the first
source that has it **for the same agent and the same episode** (a blocked
stretch or a working turn, bounded by the winner's own transitions).

| Field | Order |
|---|---|
| `reason.kind` | R `kind`; else H (Claude Notification type, opencode `permission.asked` / `question.asked`); else none |
| `reason.text` | R `msg`; else H detail (Codex/Claude PermissionRequest tool as "approve <tool>"); else the kind's copy |
| agent (display) | H source; else R `app`; else P |
| session | H session id; later R `session=` if the spec gains it |
| title | R `title`; else H session title |
| model | H metadata |
| progress | R `progress`; else N 9;4 |
| children | R children (bounded list); else H `background_tasks` count |

"Same agent" means `app_for_source(H.source) == R.app`
(`crates/roost-engine/src/program_status.rs:604`), or a P identity that
maps to both. On conflict the process in front wins identity; the other
side's text is dropped and counted.

### End rules

| Thing | Ends when | Change from today |
|---|---|---|
| Live record | `clear`; prompt A/B/D; **its stamped process exits**; RIS | + process exit (spec "must"; #519) |
| Root `idle` authority | as today; + process exit | + process exit |
| Kept `done`/`error` | **the user has seen the tab**, or the next `133;C` | + seen (spec, libghostty, herdr, cmux) |
| Child records | `clear`; process exit; over the cap the oldest is evicted | evict instead of refuse (`program_status.rs:329` refuses today) |
| Hook ownership | release; supersede; PTY replaced; **its stamped process is gone** (not merely stopped) | reverses DL-14's "label survives" (#621). Ctrl-Z keeps it. In user terms: [Ownership after an agent dies](#ownership-after-an-agent-dies) |
| Hook lifecycle | as today; + owner's process gone | fixes "stuck running" without marks (#519) |
| Late hook from a dead process's session | refused until a new claim from a live process | herdr's rule |

The stamp is `(pid, start time)`, taken through the pid-reuse-safe path
`ForegroundLeader` already uses for cwd (`pty.rs:2377-2420`). A record is
stamped with the leader at receipt, an owner with the leader at its
claim. The process is sampled at every prompt mark, hook report and tab
focus, and on a 1 s tick **only** for tabs with live state. That is
evidence, not a TTL, so DL-14's no-TTL rule stands. A record over plain
ssh is stamped `ssh` and ends when ssh exits.

### Attribution by peer process (#623, #624)

`tab.agent_report` arrives on a Unix socket. The kernel names the caller.
`crates/roost-ipc/src/peer.rs:36` reads `SO_PEERCRED` but keeps only the
uid; macOS uses `getpeereid`, which has no pid, so it needs
`LOCAL_PEERPID` as well.

1. **Same session.** portable-pty makes each tab child a session leader
   (`pty.rs:2368`). A peer whose session id is not the tab child's pid is
   not in the tab: a `setsid` daemon such as the codex app-server of
   #624. Drop it as a new reason, `not_in_tab`.
2. **Nearest agent ancestor.** Walk the peer's parents. If an agent
   process sits **below** a different foreground agent, the claim is
   nested. This covers same-agent nesting (`claude -p` inside a Claude
   turn, #623) with no `SessionStart.source` field.
   `is_nested_claim`'s source comparison
   (`crates/roost-ipc/src/agent.rs:566`) becomes the fallback.

Limits:

- A hook from a tmux, screen or zellij pane runs in the multiplexer
  server's session, so check 1 fails it. That is arguably right (the
  #624 class), but it changes behaviour (Inferred). Observe first.
- A forwarded socket names sshd. Fall back to today's rules there.
- Whether each agent runs its hooks in the tab's session is unverified.
  Phase 0 checks it.

**A new `DropReason` is a soft degrade, not a break.** The enum is closed
(`agent.rs:379`), but `Tab.dropped_reports` decodes leniently
(`messages.rs:3631`): an older client reads the whole record as absent and
keeps the tab. No protocol bump is needed.

**Observe-only needs its own counter.** `Tab.dropped_reports` is one total
plus the latest drop (`DroppedReports { count, last }`, `agent.rs:410`).
Ordinary `not_owner` drops overwrite `last`, so it cannot show how often a
check *would* have dropped. Count would-drops per reason separately (a log
line per stream, as `record_drop` does at `workspace.rs:3215`, plus a
doctor counter).

### What each state shows

| Composed state | Decided by | Dot | Agents panel row | Banner | `tab.state` |
|---|---|---|---|---|---|
| Working | R working; H working; N 9;4; S `C` | working | "Working · <msg or tool>", a thin progress bar when known | none | `running` |
| Blocked, permission | R `kind=permission`; H PermissionRequest | waiting | "Needs approval · <tool or msg>" | "<Agent> needs approval", body = reason text | `needs_input`. `tab prompt` refuses without `--force` |
| Blocked, question | R `kind=question`; H question asked | waiting, own glyph | "Has a question · <msg>" | "<Agent> has a question" | `needs_input`. `tab prompt` allowed |
| Blocked, auth | R `kind=auth` | waiting | "Needs sign-in" | "<Agent> needs sign-in" | `needs_input` |
| Blocked, no kind | R blocked; H Notification | waiting | "Needs your input · <msg>" | "Needs your input" | `needs_input` |
| Done, unseen | R root `done`; H Stop | finished | "Done · <msg>" until the tab is seen | "Turn complete" (today's copy) | `idle` |
| Error | R `error`; H StopFailure | failed | "Failed · <msg or class>" | "<Agent> failed" | `idle` |
| Idle | R `idle`; H owner, inactive | none | "Idle" | none | `idle` |
| Running, identity only | P | none | "<Agent> · running" | none | `none` or shell state |
| Suspended | P (stopped) | none | "<Agent> · suspended" | none | `idle` |
| Exited | P exit | cleared | row goes | "<Agent> exited (<code>)" once, only when it held live state and the code is not 0 | shell state |
| Children | R child records | parent's | nested rows under the agent row (top N); a blocked child lifts its kind into the parent | per child only if blocked | unchanged |

Today's copy, for contrast: the row says "Waiting · <msg>" or "Working ·
<msg>" from the deciding record (`agent_palette.rs:227,298`); `kind` is on
the wire but never changes the copy; Codex blocks read "Needs your input".

---

## Codex without hooks

Discovery for key question 5 (second pass, 2026-11-08). **Charlie's bar:**
a hook-free Codex is acceptable as long as the Agents panel and the agents
palette update properly, show the status, and their rows are clickable
(take you to the tab).

**Answer: it already meets the bar, with no new code.** Proven from code,
from two CI tests, and live with a real Codex 0.165.0 and no hooks in an
isolated Roost dev instance. A hooked Codex already shows 7501's state
today, because records outrank hooks, so removing the hooks is almost
invisible.

### What the panel and the palette need

**Who gets a row.** One rule feeds every agent surface: the Agents panel,
the per-project sidebar rows, the palette and the mobile client all build
on `panel_rows`. `row_agent` (`crates/roost-ui-model/src/agent_palette.rs:227`)
gives a tab a row when either is true:

1. **A live hook owner:** ownership is set and its source is not `manual`
   or `legacy` (`agent_owner`, `:204`).
2. **Otherwise, a 7501 record naming an app** (plan 082 D8.1). No hook
   ownership is made up for it.

The record is present only while records decide the tab or a kept root
`done`/`error` shows (`compose`, `crates/roost-engine/src/program_status.rs:466`;
`summary`, `:490`).

| Field | Hook-owned tab | 7501-only tab |
|---|---|---|
| Agent name (palette title, `sidebar_dump`; **the iced panel row never draws it**: it shows name, age, location and status, `agents_panel.rs`) | `agent_display_name(owner.source)`: "Codex" | `app_label(record.app)` (`roost-ipc/src/program_status.rs:175`): "Codex" |
| Name line | session title (Claude only), else the tab title, else `Tab N` | tab title, else `Tab N` |
| Status: dot colour and text | `effective_lifecycle()`: the records' composed lifecycle when the tab has records, else the hook layer | same |
| Reason text | the deciding record's `msg`; under hooks alone, hook `detail` ("Working · N bg tasks", Claude) | record `msg` (`tab_status_text`, `:298`). **Codex sends no `msg`** |
| Age | last hook event | last 7501 transition |
| Model | shown nowhere in the UI; only `roostctl doctor` | n/a |
| Click | panel: `AgentSelected` → `select_agent` (`app.rs:7520`) → `focus_tab_and_clear` (`app.rs:8222`); palette: `agent:<tab>` → the same call (`app/palettes.rs:1634`). Neither looks at ownership; only a disconnected host's dimmed row is inert | same |

### Proof

- **CI:** unit test `a_record_naming_an_app_gives_an_ownerless_tab_a_row`
  (`agent_palette.rs:660`); E2E `test_a_record_only_agent_gets_a_panel_row_named_by_its_app`
  (`tools/roosttest/test_program_status.py:587`, in the curated
  `e2e-iced` lane).
- **Live, real Codex 0.165.0, no hooks (Proven).** An isolated
  `roost-iced` dev build under `tools/wayland/weston-run.sh`, its own
  runtime dir, `ROOST_CONFIG` (`agent-hooks = off`), state, `HOME`, XDG
  and `CODEX_HOME`, under `env -i`. The row and composition code has not
  changed since 2026-10-28.

| Moment | Composed `tab.list` state | Panel row (`app.sidebar_dump`) |
|---|---|---|
| Sign-in, trust and update screens | `running` (the shell's `C`), no record | **no row**: Codex sends nothing before its composer |
| Composer ready | `none`, record `idle app=codex`, decided by records | "Codex", name "work", **Idle** |
| Prompt submitted | `running`, record `working` | **Working** |
| Turn fails (fake API key, 401) | `needs_input`, record `error` | **Failed**, plus a banner "Codex — Failed" |
| A key pressed in Codex | record `idle` (Codex acknowledges done/error) | Idle |
| Ctrl-Z | `clear` arrives, no record | **row gone** |
| `fg` | record `idle` again | row back |
| `kill -9` mid-turn under bash (with Roost's marks) | the `D` mark drops the live record | **row gone, nothing stale** |
| `kill -9` mid-turn under `/bin/sh` (no marks) | stuck `running`, record `working` | **stuck "Working"**; a raw OSC 9 printed afterwards was muted |

Palette and click, live: `palette open --kind agents` listed the tab as
"Codex · Untitled 1 · work", Idle, and `palette activate agent:3` moved
the active tab to it. A panel row could not be pressed directly (headless
weston has no input devices); it is the same `focus_tab_and_clear` call.

The last row is not Codex-specific: it is the dead-agent problem of
[Ownership after an agent dies](#ownership-after-an-agent-dies), and
Phase 3a fixes it for records and hooks alike.

### What session id and ownership buy, in user terms

Charlie asked how these relate, and whether they are "the write path".
**They are not.** The write path is the files Roost puts in `~/.codex`.
Session id and ownership are bookkeeping inside Roost:

- A hook report arrives over Roost's socket from a short-lived helper
  process. Nothing in the report proves which agent in which tab sent it.
  So the first report (SessionStart) *claims* the tab: Roost notes
  "Codex, session abc, owns tab 3", and from then on accepts reports from
  that session and drops reports from any other. That is **ownership**.
  It exists only because hook reports arrive out of band.
- 7501 needs none of it. It arrives inside the tab's own output, so it
  belongs to the tab by construction. With no hooks there is nothing to
  own.

| What it buys | Hooked Codex today | 7501-only Codex |
|---|---|---|
| The agent name on the row | "Codex" | "Codex", from `app=codex`. The panel row shows the tab title either way |
| A nested agent can't take the tab (#623) | refused only while Working or Waiting (`is_nested_claim`, `roost-ipc/src/agent.rs:566`) | **Still protected, more strongly**: while Codex's record decides, another agent's claim is dropped as `ForeignApp` (`program_status.rs:553-565`), even at Idle. A nested `codex exec` is not the TUI and sends no 7501 (Source) |
| Duplicate notifications muted while the agent runs | yes: records decide, so raw OSC 9/777/99 are muted (`suppress_raw_osc`) | **same** |
| `roostctl wait`, `tab prompt` | read the composed `tab.state` | **same** (Proven live: `none` → `running` → `needs_input`) |
| Approval banner text | likely "Needs permission to use `Bash`" (Inferred: the hook may report before the overlay draws) | "Codex — Needs your input" |
| Resume on restore (Phase 7, not shipped) | could run `codex resume <session_id>` | **lost** until Codex puts a session id in 7501. `codex resume --last` filters by cwd, so it is wrong when two Codex tabs share a directory |
| `roostctl doctor` | owner, model, session lines | those lines go |

### Options against the bar

| | Meets the bar? | Size | Notes |
|---|---|---|---|
| **(a) 7501 only** | **Yes, today** (Proven) | **S** for Codex itself: stop wiring it, retire its entries with Phase 2's machinery, show Codex as "Built in" on the card, say "status via terminal" in doctor and the docs | gaps below |
| (b) `codex()` shell function with `-c` hooks and a Roost-computed `hooks.state` | yes, plus the approval tool name and a session id | **M** | Interactive bash and zsh only (not fish, command-launched tabs, Apple bash 3.2); loses to a user's own alias or function; `command codex` bypasses it; trust hashes are version-coupled, and a drift shows "Hooks need review" on every launch and "Trust all" writes the user's `config.toml`. **New (Source):** Codex runs hook commands in a new session and threads in a detached daemon, so Phase 3b's same-session check would class every Codex hook `not_in_tab` (#624). Keeps the stale-owner problem |
| (c) keep writing `~/.codex` | yes | S (status quo) | against the goal |
| **(d) 7501 for state, plus Phase 3a's process identity for presence and exit** | yes, and fills (a)'s presence gaps | **S on top of Phase 3a** (3a is L, shared by every agent) | adds "Codex · running" on the sign-in, trust and update screens, "Codex · suspended" on Ctrl-Z, and ends the stuck "Working" after a crash without marks. Does not help inside tmux |

### Recommendation

**(a) now, growing into (d) when Phase 3a lands. No shell function.**
Send Codex the upstream ask: put the approval and activity text it
already computes into 7501 `msg`, and a `session=` key.

What changes for a user compared with today's hooked Codex:

- **Unchanged:** panel, sidebar and palette rows appear and update with
  the same words (Idle, Working, Waiting for input, Finished, Failed);
  clicking a row focuses the tab; banners for waiting, finished and
  failed; `roostctl wait` and `tab prompt`; duplicate notifications muted
  while Codex runs; nested agents can't take the tab.
- **Lost until Phase 3a or an upstream change:**
  - an approval banner reads "Needs your input", not "Needs permission
    to use `Bash`";
  - no row while Codex is on its sign-in, trust or update screen;
  - the row disappears on Ctrl-Z and returns on `fg`;
  - doctor no longer shows a Codex owner, model or session;
  - a future resume-on-restore can't name the exact Codex session;
  - Codex inside tmux or zellij in a Roost tab gets no row (hooks reached
    it through the inherited environment; tmux does not pass 7501
    through, Inferred).
- **Gained:** nothing in `~/.codex` (no trust-review dialogs, no `sh` and
  `cat` per event); after a crash under bash, zsh or fish the row goes
  away instead of staying as a stale "Codex · Idle" (Proven, both sides);
  the age counts from the last state change; it works over plain ssh to a
  machine without Roost.

---

## Ownership after an agent dies

Key question 12, in user terms; **decided yes** on 2026-11-08. The question was: should Roost stop
treating an agent as the tab's owner once the agent's process is gone?

**What you see today** (Proven live with a fake hooked agent; the same
rule applies to Claude, Codex, Grok and cursor):

1. **The agent crashes or exits without saying goodbye, in bash, zsh or
   fish** (`kill -9`, a crash, an `/exit` that skips the end hook as in
   grok-build#26, a hook that timed out). The shell's prompt mark drops the state to Idle, but Roost
   keeps the dead agent as the tab's owner, as a "label" (DL-14). The
   Agents panel and the palette keep a row reading **"<agent> · Idle"**
   for a program that no longer exists, until the tab closes or another
   agent claims it. Clicking it just takes you to a shell prompt.
2. **The same thing in a tab with no prompt marks** (`sh` or dash, a tab
   started with a command, macOS's `/bin/bash` 3.2, a shell with
   integration off). The tab is **stuck "Working"** with a busy dot.
   `roostctl wait` and `tab prompt` hang until their timeout.
   **Notifications from the next program in that tab are silently
   dropped**: a raw `OSC 9` fired nothing. The only fix today is
   `roostctl tab set-state --tab N --state none`. A 7501-only Codex killed
   under `/bin/sh` does exactly the same (Proven).

**What changes.** Roost ends ownership, and any live state, once it sees
in the process table that the agent is gone: the PTY's foreground process
group is back to the shell, or the stamped pid has exited. A prompt mark
alone is not enough. A stopped agent (Ctrl-Z) keeps ownership.

- Stale "Idle" rows disappear; a new agent can take the tab at once;
  notifications are never muted by a dead agent.
- Nothing useful is lost: today the label's only visible effect is the
  misleading row.
- Hooked agents then behave the way 7501 rows already do in case 1.

**Risk.** A wrapper that forks the real agent and exits (a `node` shim)
could look like an exit; follow the process group, not only the leader
pid (Phase 3's false-release risk). DL-14 and the shared state fixtures
change.

**Decided (Charlie, 2026-11-08): yes.** A small first step is #621 (release at an A/D
mark when the foreground group is the shell), which fixes case 1. Phase
3a's process stamps fix case 2 for hooks and records alike (#519).

---

## Migration off config writes

### Principles

Charlie, 2026-11-08 (**decided**): "If no longer needed Roost should
remove entries from the files automatically… We will want to avoid
changes that we aren't confident were made by Roost."

1. **Needed means on and delivered by file.** An entry Roost wrote is
   needed only while its agent is switched on and this build delivers
   that agent by file. Everything else Roost wrote is removed
   automatically, whatever the consent key says, `off` included. That
   reverses plan 064 §3.2's "startup never removes" for the retire only;
   the startup `ensure` for file agents still never removes. One
   exception: while the key is unanswered, a file agent's entries stay
   until the user answers, so an answer of "yes" does not churn the file
   (remove, then re-add).
2. **Proof, not intent.** Roost removes only what it can prove it wrote:
   exact bytes in exactly the shape a release wrote, a whole file that
   passes its strict ownership check, a trust key with Roost's canonical
   hash. Anything else is left and named. See [What Roost may
   remove](#what-roost-may-remove).
3. **Exact match forever.** Keep `owned_commands` v1–v3, the opencode
   marker and a frozen table of every shipped opencode plugin image
   indefinitely. A synced dotfile can bring an old entry back years
   later.
4. **Back up first; never reformat.** Every file the retire writes or
   deletes is backed up first. A JSON file Roost's printer cannot
   reproduce byte for byte is not written at all: removing one handler
   must not reformat the user's text (`lib.rs:28-36` says the
   reformatting cannot be undone).
5. **Once per version, never ping-pong.** The retire runs once per home,
   agent and integration version. Entries that come back afterwards are
   reported, not chased. Two Roost builds on one machine must not take
   turns adding and removing entries.
6. **No protocol bump.** The session generation gate is exact
   (`docs/reference/ipc-compatibility.md`, "Capability negotiation").
   `session.set_agent_hooks` stays on the wire; a new session only
   reinterprets it.
7. **Duplicates are tolerated and made harmless.** Correctness must not
   depend on removal finishing first.

### Config and consent

**One source of truth: `agent-integration`** in `config.conf`. A list,
`off`, or absent (the defaults: env and 7501 agents on, file agents off;
decided 2026-11-08, see [The startup
prompt](#the-startup-prompt-agent-integration)). Unknown tokens are kept
(#486's rule). The UI, the local session, a host session, `roostctl` and
doctor all read only this key for decisions. A Phase 1 detail to settle:
a user who switches one agent off should not, by writing a full list,
also opt out of agents a later release adds; recording only the
departures from the defaults avoids that.

**The legacy `agent-hooks` key becomes a shadow for older builds.** A new
build reads it once, to carry the answer over, and afterwards only writes
it: the agents in `agent-integration` that this build delivers by file,
or `off`.

- An older build's startup `ensure` wires only what that key lists and
  never removes (`ensure.rs:328`; `plan` skips unlisted agents as
  `NotAllowed`, `:292`). With the shadow narrowed, it neither re-adds
  Claude nor fights the new build.
- The cost: an older build on the same machine loses Claude and opencode
  hooks. 7501 still covers Claude's state.
- One key with a new meaning would not work: an older build would read
  `agent-hooks = claude` as "wire Claude by file" and put the entries
  back. Two keys with one reader is what prevents the add-and-remove
  loop. Doctor reads `agent-hooks` only to say "an older Roost widened
  agent-hooks".

**Carry-over, first launch of the new build:**

| On disk before | `agent-integration` | Card | Retire | Notice (once) |
|---|---|---|---|---|
| Neither key (fresh install, or an earlier "Decide later") | left absent: the defaults (env agents on, Cursor off) | **only if Cursor is installed** (its opt-in); otherwise none | exact entries of every agent no longer delivered by file (an old "Decide later" left none); Cursor's wait for an answer | — |
| `agent-hooks = off` | `off` | no | every exact entry | toast: "Roost can now connect … without editing their settings files. They stay off" |
| `agent-hooks = <list>` | the same list | no (Cursor keeps its earlier answer) | every agent no longer delivered by file | toast: "… now connect without changes to their settings files. Roost removed …" |
| `agent-integration` set | unchanged | no | per its value | — |

This has to be in Phase 1, before any tab is spawned with the plugin
variables. Otherwise a user who said `off` gets Roost's hooks back in
every tab, with no file touched.

**On by default, no question (decided 2026-11-08).** An earlier draft
connected nothing until the user answered a card, because env delivery
still runs a Roost process inside the agent on every event, Claude
records its own plugin state (caveat 6), and a sideload policy makes
Claude exit (caveat 2). Charlie's second-round answer: show a card only
when a user file would change. The other costs are met without a
question: the sideload probe skips Claude where a policy would make it
exit (caveat 10), doctor names every agent's delivery, and each agent has
an off switch in "Agent integration…", `config.conf` and `roostctl agent
set`. A user who once chose "Decide later" is now on the defaults; that
user had no entries, so nothing is retired and no toast is due.

**Hosts.** A host session reads its own `agent-integration`.

- A connecting v4 client sends its whole list; a v4 session records the
  env agents only where the host never answered (no raise over `off` for
  env agents), and handles file agents as today.
- A v4 client sends an older session only its file agents. An older
  session would wire anything it is sent by file.
- A v4 session accepts an empty list, so a client with nothing on still
  gets the host's retire notice; an older session refuses one, so the
  client sends it only to v4.

### The startup prompt: Agent integration

Charlie asked for the popup that asks which integrations to turn on to be
updated. In the second round (2026-11-08) he **decided** it appears only
when it has something to ask: a file-writing opt-in for an installed
agent. This is the design.

**Today's card** (`crates/roost-iced/src/app/agent_hooks_dialog.rs`), for
contrast:

- Title "Agent hooks" (`:27`). Lede: "Roost adds a hook to each agent you
  switch on so its tabs show status and send notifications. It edits the
  agent files named below, plus Roost's own config, and nothing else.
  roostctl agent uninstall --all puts the agent files back." Notes for
  Codex (pre-trusts in `config.toml`) and OpenCode (a plugin).
- All five agents always drawn, each with a switch, a "found here" chip,
  the files Roost would touch and, in preferences, a status line
  (`row_status`, `:158`). A switch starts on if the agent is installed
  and the key is unanswered (`prefill`, `:143`).
- Buttons: "Decide later" and "Instrument N" (or "Turn off"); in
  preferences "Cancel" and "Apply".
- Flow: `window_opened` (`app.rs:4415`) → `spawn_ensure`
  (`app/agent_hooks.rs:156`); `claim_first_run` (`:134`) raises the card
  once per process when the key is unanswered and the test fence is down;
  `survey_verdict` (`agent_hooks_dialog.rs:413`) shows it only if some
  agent is installed. "Decide later" writes nothing, so the card returns
  next launch. Confirm sends `agent.set_hooks`. Preferences opens from
  "Agent Hooks…" in the palette (`roost-ui-model/src/palette.rs:238`) and
  the Mac menu (`macos/menu.rs:712`).

**When the card appears** (decided 2026-11-08: only when it has
something to ask):

| State on disk | Card? | Instead |
|---|---|---|
| An installed agent needs a user file changed and its answer is not recorded (today only Cursor: installed, and `agent-integration` absent) | **yes**, once per process, until answered | — |
| No installed agent needs a user file changed | **no** | nothing to ask: env and 7501 agents are on (the defaults). A toast only if the sideload probe skipped Claude (below) |
| `agent-hooks = <list>` (had hooks on) | **no**: the answer carries over, Cursor's included | automatic retire, one toast |
| `agent-hooks = off` (said no) | **no**: `off` carries over | retire of any exact leftovers, one toast |
| `agent-integration` set | never again | — |
| `ROOST_TEST_MODE` without the fence lifted | no | — |

Cursor installed later, after a first run with no card: the card appears
at the next launch, because Cursor's answer is still unrecorded. Not
re-asking upgraders is deliberate: their answer maps exactly onto the new
key. No modal ever explains the migration; upgraders get toasts.

**Rows, by delivery in this build:**

| Agent | Delivery | First-run card | Preferences ("Agent integration…") |
|---|---|---|---|
| Claude Code | env plugin `roost` | not on the card | switch, **on** by default. Shown off with the reason when the sideload probe finds the policy |
| OpenCode | env plugin | not on the card | switch, on |
| Codex | 7501 (built in) | not on the card | switch, on, labelled "Built in" |
| Grok | 7501 (built in) | not on the card | switch, on, labelled "Built in" |
| Cursor | file (merge into `$HOME/.cursor/hooks.json`) | **the card's one row, unchecked** | switch, off by default |
| crush, pi | 7501 (built in) | not on the card | one combined "Built in" line |

The off switch for an env agent stops injecting its variable. For a
built-in (7501) agent there is nothing to stop delivering; off means Roost
ignores that agent's records for rows and banners (a proposal; small, in
the composition layer).

**Copy (draft):**

- **Title:** "Agent integration"
- **First-run lede** (names only installed agents): "Roost shows what
  your coding agents are doing (working, needs you, done) in the sidebar,
  the Agents panel and notifications. Claude Code, Codex and Grok are
  already connected, with nothing written to their settings. One agent
  needs a change to its own settings file to report fully:"
- **Cursor:** "Adds Roost's entries to ~/.cursor/hooks.json, because
  Cursor reads its turn-end hook only from that file. Without it, Roost
  shows only that Cursor is running." (The last clause needs Phase 3a;
  before that, "shows nothing for Cursor".)
- **First-run footer:** "Change this, or switch any agent off, from Agent
  integration… in the command palette."
- **First-run buttons:** "Decide later" (writes nothing; the card returns
  next launch) and "Continue" (records the answer; Cursor is connected
  only if checked).
- **Preferences lede:** "Roost shows what your coding agents are doing in
  the sidebar, the Agents panel and notifications. Only rows that say so
  change an agent's own settings."
- **Claude Code:** "Connected inside Roost tabs only. Nothing is written
  to ~/.claude. Needs Claude Code 2.1.280 or newer." Under a policy:
  "Not connected: your organization's Claude Code policy blocks plugin
  folders (disableSideloadFlags). Status still shows from the terminal."
- **OpenCode:** "Connected inside Roost tabs only. Nothing is written to
  ~/.config/opencode."
- **Codex, Grok (built in):** "Built in. <Agent> reports its status to
  the terminal itself."
- **Built-in line:** "Also built in: crush and pi report their status on
  their own."
- **Preferences footer:** "Hosts you connect to follow this choice unless
  they have their own."
- **Preferences buttons:** "Cancel" and "Apply".
- **Preferences status lines:** "Connected · nothing written"; "Off";
  "Skipped: blocked by your organization's Claude policy"; "Wired v3 ·
  ~/.cursor/hooks.json"; "Old Roost entries left in <file> (you changed
  them) — Remove…"; "Came back after Roost removed it (synced from an
  older Roost?) — Remove…".

**Mockup, first run** (only when Cursor is installed):

```
┌─ Agent integration ──────────────────────────────────────────────┐
│ Roost shows what your coding agents are doing (working, needs    │
│ you, done) in the sidebar, the Agents panel and notifications.   │
│ Claude Code, Codex and Grok are already connected, with nothing  │
│ written to their settings. One agent needs a change to its own   │
│ settings file to report fully:                                   │
│                                                                  │
│ [  ] Cursor                   found here                         │
│      Adds Roost's entries to ~/.cursor/hooks.json, because Cursor│
│      reads its turn-end hook only from that file. Without it,    │
│      Roost shows only that Cursor is running.                    │
│                                                                  │
│ Change this, or switch any agent off, from Agent integration…    │
│ in the command palette.                                          │
│                                  [ Decide later ]  [ Continue ]  │
└──────────────────────────────────────────────────────────────────┘
```

**Mockup, preferences** (from the palette or the Mac menu):

```
┌─ Agent integration ──────────────────────────────────────────────┐
│ Roost shows what your coding agents are doing in the sidebar,    │
│ the Agents panel and notifications. Only rows that say so change │
│ an agent's own settings.                                         │
│                                                                  │
│ [■■] Claude Code    found here   Connected · nothing written     │
│      Connected inside Roost tabs only. Nothing is written to     │
│      ~/.claude. Needs Claude Code 2.1.280 or newer.              │
│ [■■] OpenCode       found here   Connected · nothing written     │
│ [■■] Codex          found here   Built in                        │
│ [■■] Grok           found here   Built in                        │
│ [  ] Cursor         found here   Off                             │
│      Adds Roost's entries to ~/.cursor/hooks.json, because Cursor│
│      reads its turn-end hook only from that file.                │
│  ──  Also built in: crush and pi report their status on their own│
│                                                                  │
│ Hosts you connect to follow this choice unless they have their   │
│ own.                                           [ Cancel ] [Apply]│
└──────────────────────────────────────────────────────────────────┘
```

**What an upgrader sees** (each toast once, through the record's notice
flag, as `noticed` today; never a modal):

- **Had hooks on, clean removal:** "Claude Code and OpenCode now connect
  to Roost without changes to their settings files, and Codex and Grok
  report their status on their own. Roost removed the entries it had
  added (backup in ~/.config/roost/agent-backups). Agent integration… to
  change." (Names only the agents whose entries were retired.)
- **With leftovers:** the same, plus "Roost left 1 entry it can't be sure
  it added: roostctl agent status lists it."
- **Had `off`:** "Roost can now connect Claude Code and OpenCode without
  editing their settings files. They stay off; turn them on under Agent
  integration…". If the retire removed exact entries: "Roost also removed
  hook entries it had added earlier (backup in
  ~/.config/roost/agent-backups)."
- **A host:** "on <host>: Roost removed its old agent entries there
  (backup on <host> in ~/.config/roost/agent-backups)."
- **Claude skipped by policy** (anyone, once per machine while it holds):
  "Claude Code isn't connected: your organization's Claude policy blocks
  plugin folders. Its status still shows from the terminal." Doctor keeps
  the line.

"Continue" and "Apply" still go through `agent.set_hooks`, which now
writes `agent-integration` and the shadow, and sends `off` when every
switch is off. "Continue" records the defaults plus Cursor's answer.

### The retire step (integration version 4)

Runs automatically, once per machine per integration version per agent,
under `ConfigLock`. Triggers: the UI's startup path, beside `ensure`, off
the UI thread; and `roost-session` start on a host (new), so a host comes
clean even if no upgraded client connects. Respects the test `Guard`
(`plan.rs:100-103`).

**Wait for the machine's session.** A `roost-session` survives a UI
upgrade: the UI only offers "Restart the session on <host>?" on a build
mismatch (`crates/roost-iced/src/app/host_notice.rs:123`), and the user
can decline. An older session builds its tabs' environment without the
plugin variables, so on that machine the file entries are still needed.
The fresh-install default puts local tabs on a managed local session, so
this is the common case, not a corner. Rule: the UI retires only when no
older `roost-session` serves this machine (it knows the local session's
build). Otherwise it defers to that session's own v4 start. Doctor names
the wait. The new UI never edits a remote host's files; that host's own
v4 session does.

Per agent with entries that are no longer needed (see
[Principles](#principles)):

1. **Decide.** Skip if the record says `retired: 4` for this agent, under
   the test `Guard`, while an older session serves this machine, or for a
   file agent while the key is unanswered.
2. **Plan, strictly.** A new `Intent::Retire` across the current home and
   every home the record names (`unwire_homes`, `ensure.rs:832`), using
   the predicate in [What Roost may remove](#what-roost-may-remove).
   Everything else becomes a leftover with a reason.
3. **Check the layout.** Re-render each JSON file unchanged. If the bytes
   differ from the file, the edit would reformat it: skip the file, as
   leftover `would_reformat`. TOML goes through `toml_edit` and needs no
   check.
4. **Back up** each file about to change, before any write, to
   `~/.config/roost/agent-backups/v4/<agent>/<sha8(path)>-<basename>.<unix-ts>`,
   plus an `index.json` mapping each backup to its original path. Mode
   0600, directory 0700, fsynced; keep the last 3 per original path. The
   folder sits beside the record under `$HOME/.config/roost`
   (`home.rs:159-166`). New code: `write.rs` has only the staged temp
   file today.
5. **Apply** with the existing `apply`.
   - A changed-underneath skip (Claude rewrites `settings.json` itself)
     retries at the next launch; three misses in a row make it a
     leftover.
   - A file Roost created that ends up empty is deleted by the existing
     `created` rule (for cursor, a lone `version` counts as empty,
     `cursor.rs:83-93`).
   - Codex: the trust keys go with their handlers, and `[features] hooks`
     is restored from the recorded pre-image only when no other hooks
     remain (`codex.rs:613-644`).
   - The record is written per agent after its files, so a cut retire
     (#553 item 2) re-runs from the files left, with no second backup of
     a finished agent.
6. **Keys.** Write `agent-integration` (carry-over) and the narrowed
   `agent-hooks` shadow.
7. **Record** `{retired: 4, retired_at, backups, leftovers: [{file, event,
   reason}]}` per agent. This is the tombstone #485 asks for.
8. **Tell the user once**, through the record's notice flag, toast first
   (the `noticed` order, `agent_hooks.rs` module doc). Copy is in [The
   startup prompt](#the-startup-prompt-agent-integration).
   - On a host, the next connecting v4 client's toast carries it, through
     the reply to `session.set_agent_hooks` (`Notice::Here`,
     `ensure.rs:506`). An older client logs the reply and shows nothing.
   - Leftovers are never toasted again (they are not faults, so they do
     not use `problem_line`'s every-launch rule). Doctor, `agent status`
     and the preferences card keep naming them.

**After the retire.** A cheap, read-only check at startup notices exact
entries that came back (a synced dotfile, an older build elsewhere). It
reports them as `came_back` in doctor, `agent status` and the preferences
card ("Remove…"), and does not remove them again until the next
integration version. Chasing them every launch would loop with an old
build on another machine through the sync, rewriting the user's file
each time.

Failure modes:

- an unparseable file is retried quietly at each launch, with a doctor
  line;
- a lost record re-runs a retire that finds nothing, so no backup and no
  toast;
- a `CLAUDE_CONFIG_DIR` set in only some shells is found through the
  record's file list, as `off` is today. One Roost never recorded cannot
  be found.

How the user hears about leftovers, besides the one toast: a doctor row
per agent, for example `claude: delivery env · retired v4 2026-11-20 ·
backup ~/.config/roost/agent-backups/v4/claude/… · 1 entry left in
~/.claude/settings.json (annotated: matcher "Bash"); roostctl agent
uninstall claude removes it`; and `roostctl agent status [--json]` with
`delivery`, `retired {version, at, backups[], leftovers[{file, event,
reason}]}` and `came_back`.

### What Roost may remove

**Removed automatically** (Roost can prove it wrote it):

| Where | The proof |
|---|---|
| A handler in Claude's `settings.json` or Codex's `hooks.json` (grouped: `hooks.<event>[group].hooks[handler]`) | `command` byte-equal to an `owned_commands` string (`command.rs:141`); handler keys only `type` (`"command"`), `command`, `timeout` (and `async`, which no release has written), with values a release wrote for that agent and event (a frozen table built from the release tags; the writer is `jsonedit::handler`, `jsonedit.rs:147`); a group with no key besides `hooks`; the first such handler on its event (the writer keeps one per event, `merge_grouped`, `:220`) |
| A handler in cursor's `hooks.json` | the same handler test. Cursor's shape is flat (`hooks.<event>` is a list of handlers, no groups; `merge_flat`, `cursor.rs:1-7`), so the group clause does not apply |
| An emptied group, event or `hooks` object | `remove_grouped` / `prune_empty_hooks` (`jsonedit.rs:274-347`) |
| A whole file Roost created, once empty | the record's `created` list |
| `$GROK_HOME/hooks/roost.json` | strict `is_ours` (`grok.rs:70`): only Roost's events, groups and handler fields, every command Roost's |
| opencode `plugins/roost-agent-state.js` | the marker line (`opencode.rs:55`) **and** a byte match against the frozen table of shipped plugin images (new; local edits are overwritten by design today) |
| A Codex `[hooks.state."<key>"]` entry | the key's path is this `hooks.json`'s key source, and `trusted_hash` equals Roost's **canonical** hash (`our_hashes`, `codex.rs:131`, without its on-disk-identity candidate). Removed with its handler, or alone when the handler is already gone: a trust entry for a command that is not there runs nothing |
| Codex `[features] hooks` | restored to the record's pre-image, only when no other hooks remain (existing) |
| Roost's own state | the record, backups, and the two keys in `config.conf` |

**Left in place and reported** (not provably Roost's):

| Case | Reason |
|---|---|
| An owned command whose handler has another key, a changed `timeout`, or `async: true` | `edited` |
| A handler in a group with a `matcher` or any other key (the user scoped or annotated it) | `annotated` |
| A command that only looks like Roost's (`looks_edited`, `command.rs:160`); pre-plan-046 `roostctl claude-hook` entries | `edited` |
| A second exact copy on one event (a copy, a dotfile merge) | `duplicate` |
| A JSON file Roost's printer would reformat | `would_reformat` |
| Unparseable, not UTF-8, unexpected shape | `unreadable` (retried each launch, quietly) |
| A file changed between plan and rename | retried next launch; a leftover after 3 misses in a row |
| `roost.json` failing `is_ours`; an opencode file with the marker and an edited body | `foreign`, `edited` |
| A Codex trust key with a non-canonical hash (an edited handler's, or the user's own) | kept beside its handler |
| `[features] hooks` with no pre-image in the record | kept. Codex 0.165.0 does not need it (x7); a leftover `true` runs nothing |

**Key order is not a test.** JSON objects carry no order, and any tool
that rewrites the file (Claude itself, `jq`, an editor) can reorder keys
without a person touching the entry. Comparing as a set loses nothing and
avoids false leftovers.

**Explicit removal keeps today's looser rule.** `roostctl agent uninstall
<agent>` and a new "Remove…" on the preferences row remove any handler
whose command is byte-equal to an owned command, as today, now with a
backup and refused for `would_reformat`. The automatic step is strict; an
explicit request is the user's own decision.

**How common will `would_reformat` be?** Roost's printer reproduces a
two-space pretty file byte for byte (`json.rs` test
`a_pretty_two_space_file_round_trips_byte_identically`), and that is
likely the layout Claude's own writes produce (Inferred). A read-only check on this box
(Python's `json.dumps(indent=2, ensure_ascii=False)` as a stand-in for
Roost's printer) found `~/.claude/settings.json`, `~/.codex/hooks.json`
and `~/.cursor/hooks.json` all in that layout. So the common case is a
clean retire. Phase 0 confirms it with Roost's own printer on more
machines, macOS included; a hand-edited compact file is the expected
leftover.

### The overlap window

Duplicates happen where env delivery is on and a Roost entry still sits
in a file: restored tabs spawned before the retire finishes, a skipped
file, an entry an old build re-added on a synced machine, or a user-edited
entry.

| | Option | Complexity | Notes |
|---|---|---|---|
| D1 | **Do not raise an identical pending notification again.** In the engine, skip `raise_notification` when the tab already has a pending notification with the same title and body | S | **Recommended.** No wire change. Duplicate start, prompt and tool reports are already idempotent; a duplicate release drops as `not_owner`, so every session end in the window adds a `dropped_reports` entry that doctor must not treat as a fault. Also helps on its own (cursor's repeated `stop`). The new opencode plugin's `globalThis` guard dedupes only against itself: the v3 file plugin has no guard, so both run until the file is gone |
| D2 | Inject the env only when no exact Roost entry is present | S–M | Tabs spawned before the retire keep file delivery, then lose it |
| D3 | The env plugin's command passes `--via env`; the verb drops file-delivered calls in tabs that have env delivery | S | No hooks at all if Claude rejects the plugin. A distinct command string is still worth having for doctor counts |
| D4 | The engine dedupes per `(tab, source, session_id, event)` | M | Most precise; most code |

Strict removal leaves an edited or annotated Roost entry in place, and it
still runs `agent-hook <agent>`. In such a tab every event arrives from
both the file and the env plugin until the user removes the entry. D1
covers the banners; doctor counts the duplicates and names the leftover
as the cause.

### Version skew and one-back

| Case | Behaviour |
|---|---|
| New UI, old host session (same protocol) | The client raises only file agents. A raise never removes, so the old host keeps the entries earlier clients wrote, and its tabs keep file delivery: there those entries are still needed. A host that never had entries gets none, and shows 7501 state only, until its session is upgraded. Raising Claude here instead would keep Roost writing into hosts' configs, against the goal |
| New UI, new host session | The client sends its whole `agent-integration` list, empty included. The host records env agents only where it never answered, and handles file agents as today. The host's own v4 start already ran its retire, and the reply carries its notice |
| Old UI, new session | The old client raises `claude`. The new session records env agents in the host's `agent-integration` only where it never answered, with no file writes. The reply lists them in `skipped` with reason "no file needed"; the reason is a free string (`messages.rs:4219-4228`), so the old client shows no false "wired" |
| New and old build on one machine | The narrowed `agent-hooks` shadow stops the old build re-adding. If something widens it again, doctor names it |
| Downgrade to an older build | The older build reads the narrowed shadow and does not re-add Claude. Claude in its tabs shows 7501 state only until the user runs that build's `roostctl agent install claude` |
| Synced `~/.claude` from a machine on an old build | Entries may come back. The retire does not chase them (once per version); the startup check reports them as `came_back`; D1 makes duplicates harmless; "Remove…" or `roostctl agent uninstall claude` cleans them |
| Host with no Roost (plain ssh) | Nothing is written, ever. 7501 rides the ssh session |
| New UI, older local `roost-session` still running | The UI defers the retire (see [The retire step](#the-retire-step-integration-version-4)). Its tabs keep file delivery until the session restarts on v4, whose start does the retire |
| Agent started outside any Roost tab | Today Roost's entries run `sh` and print `{}`. After the retire nothing runs. A program launched **from** a Roost tab (an editor, a tmux server) inherits the plugin variables and `ROOST_TAB_ID`, so a Claude in its terminals reports to that tab. That is today's behaviour with `settings.json` too; Phase 3's `not_in_tab` check is what ends it |

**Where the plugin lives.** The injection belongs in `roost-engine`
(`tab_environment` / the env block at `pty.rs:2686-2740`), which both the
UI and `roost-session` run (`crates/roost-session/src/serve.rs:288`). The
embedding process hands in a directory it owns, written the way
`roost_resources_dir` writes shell integration under
`$XDG_CACHE_HOME/roost/` (`pty.rs:2561`). Version the folder so a running
`claude` keeps its own, and rewrite it at spawn if a cache clean removed
it. Keep the plugin's **name** constant across versions: `roost`, the
marketplace plugin's name, so inside a Roost tab it replaces an installed
`roost@roost` (caveat 9). The folder carries the hooks and
`skills/roost/SKILL.md`. Strip inherited Roost entries from both
variables before appending (the second gap above). This is consistent with DL-32: these are server variables,
not opener ones.

### Rollback

- **Per file:** every file the retire changed has a backup under
  `~/.config/roost/agent-backups/v4/` with an `index.json`. Doctor and
  `roostctl agent status` print the path and the `cp` that restores it.
  There is no restore verb: a restore would overwrite whatever the user
  or the agent changed since.
- **Per user:** none for Claude. Decided 2026-11-08: no `--file`
  fallback and no support below Claude 2.1.280. If env delivery ever
  fails in the field, the marketplace or plugin-install route can be
  revisited; it is not pursued now. Cursor keeps its file writer as its
  opt-in.
- **Per release:** a point release can stop injecting and leave the
  retire alone. 7501 still covers Claude's state. A release does not
  undo the retire.
- **Downgrade:** see the version-skew table.
- **Policy:** the sideload probe skips Claude where a policy would make
  it exit (caveat 10). If one slips through (a server policy not cached
  yet), Claude's own message says to unset the variable, and the user
  switches Claude off in "Agent integration…". No argv fallback (decided,
  key question 8).

### What `roostctl`, the UI and the docs become

| Surface | After |
|---|---|
| `agent status` | per agent: delivery (`env`/`file`/`7501`/`none`), 7501 seen, files Roost touched, `retired {version, at, backups}`, `leftovers [{file, event, reason}]`, `came_back` |
| `agent set <list\|off>` | sets `agent-integration` and the `agent-hooks` shadow; raises hosts per the version-skew table |
| `agent install <a>` | env and built-in agents: "nothing to install", pointing at `set`. Cursor, the one file agent, keeps the writer (fixed to write `$HOME/.cursor/hooks.json`). No `--file` for Claude (decided) |
| `agent uninstall <a>\|--all` | removes any Roost entries whose command is owned (today's rule), now with a backup and the reformat check |
| `agent ensure` | reconcile file agents; run the retire if pending |
| The card | "Agent integration": first run only when an installed agent needs a file opt-in (today Cursor); preferences with a switch per agent; "Remove…" for leftovers and entries that came back |
| Palette and Mac menu | "Agent Hooks…" becomes "Agent integration…"; the palette also matches "hooks" |
| Toasts | one retire notice per machine; one "now possible without editing files" for users who said `off`; one "Claude isn't connected" while the sideload policy holds; "wired" only for Cursor |
| `docs/guides/agents.md` | "Agent integration": delivery table, the retire and its strict rule, leftovers, backups, the file fallback, an honest note on what Claude writes for itself |
| `docs/getting-started/first-run.md` | no card for most users; the Cursor card, and when it appears |
| `docs/guides/agent-skill.md` | inside a Roost tab the skill arrives with the env plugin; `/plugin install roost@roost` is for Claude outside Roost tabs. Keep the marketplace `SKILL.md` identical to the env copy |
| Decision log | a new DL (next free number is DL-36) replacing plan 064's consent model, which has no DL; amendments to DL-13, DL-14 and DL-29 when Phase 3 and Phase 5 land |

---

## Roadmap

Each phase is one plan, except Phase 3, which is two. Complexity uses
`backlog-triage-2026-11.md`'s scale: S is about a day or less, M a few
days or one small plan, L a multi-commit plan, XL more than one plan.
The review raised Phases 0, 1, 2, 3 and 5 by one step each: the first
draft sized several multi-commit plans as M.

### Phase 0: Facts (M)

**Already proven** (2026-11-08 evening, isolated homes, real configs
hash-checked unchanged; see [Third
pass](#third-pass-charlies-second-answers-and-phase-0-probes)):

- 7501 re-captures of grok 1.0.77 (with child records and `app=grok`),
  Codex 0.167.0, opencode 1.19.5 (none), cursor 2026.11.05 (none) and the
  apt crush 0.97.1 (none), appended to `agent-program-status.md`.
- `disableSideloadFlags` reproduced rootless on Linux (no shed needed),
  and the `claude plugin list --json` detection probe.
- Claude's env plugin additive with installed and env user plugins; the
  name `roost` replacing the marketplace plugin; the skill loading from
  the env folder.
- cursor `--plugin-dir` with a real login: proven, partial.
- grok's cross-execution of other agents' hook files, and its trust
  prompt.

**Still open:**

- **Scope.**
  - Claude 2.1.319's 7501 with a real login (children, kinds, progress,
    auth, error), in a **temporary** `CLAUDE_CONFIG_DIR`, never the real
    one; and whether Claude emits OSC 9;4 under `TERM_PROGRAM=Roost`.
  - Claude with real auth: PermissionRequest, Notification and Stop
    through the env plugin. The probes reached only the hooks that fire
    before the auth check.
  - grok's `error` record (documented, not captured) and its "still
    running" count for work left after a turn.
  - Re-capture `gx-gate.jsonl` with grok before Phase 4 deletes it: it is
    the only evidence for the Stop-gate and subagent-drop behaviour.
  - The session id and parent chain of each agent's hook process, and of
    the codex app-server daemon, cursor's `worker-server` and the
    opencode server (#624), on Linux and macOS.
  - A full turn from pi and from crush 0.98 or newer (working, blocked).
  - The sideload probe on macOS against an MDM or `/Library/Application
    Support/ClaudeCode/` policy (root needed, so a spare Mac), and,
    if an org account turns up, against a server-managed policy.
  - Log in only on Linux. On macOS Claude keeps its credential in the
    login Keychain, which a temporary `CLAUDE_CONFIG_DIR` may not
    isolate (Inferred).
  - The retire's two facts: a Claude-written `settings.json` re-renders
    byte for byte through Roost's own printer (on Linux and macOS, more
    than one machine), and the handler timeouts each release wrote, read
    from the release tags.
  - **Probe hygiene:** put `check_for_update_on_startup = false` at the
    top of the isolated `$CODEX_HOME/config.toml` (a single Enter on
    Codex's update prompt once ran a **global** `npm install -g
    @openai/codex`; see [Second pass](#second-pass-charlies-answers)).
    Turn off every agent's updater in isolated runs: `DISABLE_AUTOUPDATER=1`
    for Claude; `"autoupdate": false` or `OPENCODE_DISABLE_AUTOUPDATE=1`
    for opencode; `GROK_DISABLE_AUTOUPDATER=1`, `--no-auto-update` and
    `[cli] auto_update = false` for grok; `"channel":"static"` in the
    isolated `cli-config.json` for cursor. Kill the Codex daemons and
    cursor `worker-server` an isolated run starts.
- **A code fix that cannot wait (S, standalone; filed as #716).** Map `app=grok` in
  `app_for_source` and `app_label`. To keep grok 1.0.70 and older working,
  let the `ForeignApp` check (`program_status.rs:553-556`) accept either
  `grok` or `grok-build` for the `grok` source. Update the tests at `program_status.rs:1132-1135`, `:1863` and
  `:1901`. Without it, grok 1.0.77's hooks are dropped as `ForeignApp`
  and its rows read `grok`. Fits backlog Plan A.
- **Why M, not S.** A logged-in Claude, a root-capable Mac, and two
  platforms for the parent-chain check.
- **Output.** Appended sections in `agent-program-status.md`.
- **Closes.** None. Feeds #622 and #624.
- **Exit.** Every "Strings" or "Inferred" row this roadmap depends on is
  confirmed or refuted.

### Phase 1: Env delivery for Claude and opencode (L)

- **Scope.**
  - A Roost-owned integration folder per process (UI, local session,
    host session), versioned.
  - The Claude plugin, named `roost`, generated from `CLAUDE_HOOK_EVENTS`,
    with the same fail-safe wrapper and a distinct `--via env` spelling,
    plus `skills/roost/SKILL.md` (decided: the skill rides in the env
    plugin). One source for the SKILL.md, so the env copy and the
    marketplace copy cannot drift.
  - The opencode plugin with a `globalThis` single-instance guard.
  - Injection in `roost-engine`, appending to a user's
    `CLAUDE_CODE_PLUGIN_DIRS` and merging a user's
    `OPENCODE_CONFIG_CONTENT` (skip and report if it does not parse).
  - bash and zsh integration re-append after the user's rc.
  - **The `agent-integration` key as the single reader**, with the
    one-time carry-over from `agent-hooks` (see [Config and
    consent](#config-and-consent)). An absent key means the defaults:
    Claude and opencode injected (decided 2026-11-08); `off` injects
    nothing.
  - The new build's own `ensure` stops wiring Claude and opencode by file
    wherever it injects them. Otherwise it keeps re-adding the entries
    Phase 2 removes.
  - Strip inherited Roost entries from both variables before appending;
    a constant plugin name.
  - **The sideload probe (decided, option A+C).** Run
    `CLAUDE_CODE_PLUGIN_DIRS=<roost dir> claude plugin list --json`
    with the serving process's environment, stdin `/dev/null`,
    `DISABLE_AUTOUPDATER=1` and a short timeout, off the UI thread, in
    each serving process (UI, local session, host session: the policy
    is per machine). rc 1 with `disableSideloadFlags` on stderr: skip
    Claude's variable, a doctor line, the one-time toast. rc 0, or no
    `claude` on PATH: inject. Anything else: inject and show "unknown"
    in doctor. The serving process's PATH may lack what the user's rc
    adds (`~/.local/bin`), so resolve `claude` as a login shell would.
    Cache the answer and re-probe at each launch and on a
    TTL (a server policy may arrive after the first fetch). Never
    `--version`. Doctor also notes a cached
    `$CLAUDE_CONFIG_DIR/remote-settings.json` as a hint. No argv
    fallback: `--settings` would get around the org's ban.
  - D1.
  - Doctor delivery rows. Doctor reads `claude plugin list` showing both
    `roost@roost` and `roost@inline` as normal (only the env copy loads).
  - The defaults with no card: env agents on when `agent-integration` is
    absent; per-agent off switches in config and `roostctl agent set`.
  - #622's new events can ride along: the plugin is generated, so no
    user file churns.
- **Closes.** #622 if it rides along.
- **Tests.**
  - Unit: env merge (empty, user value, unparseable), plugin generation
    snapshot against `CLAUDE_HOOK_EVENTS` (with the skill), the probe's
    three outcomes from a fake `claude` (rc 0; rc 1 with the marker;
    timeout).
  - Unit, carry-over table: `agent-hooks = off` → `agent-integration =
    off` and no variables; a list without `claude` → no Claude variable;
    both keys absent → the defaults (Claude and opencode variables set);
    `agent-integration` set → the legacy key is never read again.
  - Unit: an inherited value that already holds a Roost folder comes out
    with exactly one, the current one.
  - roosttest: a tab's environment carries both variables with a user
    value preserved; a jailed fake agent (`agent_jail.py`) that reads the
    plugin's `hooks.json` and runs its command with fixture payloads moves
    `tab.list` state; two identical Stop reports give one
    `NotificationFired` (D1).
  - roosttest session lane: a host-session tab carries the variables, set
    by `roost-session`, not the client.
  - roosttest: an rc that exports its own `CLAUDE_CODE_PLUGIN_DIRS` still
    ends with Roost's entry appended in bash and zsh.
  - Run each new test once with the fix removed, and record that it fails
    (the negative control).
  - Manual, agent-status harness: Claude 2.1.319 and opencode in a Roost
    tab with an empty config dir. Claude's `-p --output-format
    stream-json --verbose` init event lists `plugins[]` and `skills[]`
    (`roost@inline`, `roost:roost`) with no auth, which suits a roosttest
    assertion with a real Claude where one is installed.
  - The policy itself, rootless: the p0 `unshare -rm` overlay recipe on a
    Linux runner that allows unprivileged user namespaces (else a shed),
    asserting Roost skips the variable and doctor names the policy.
- **Exit.** In a Roost tab, Claude and opencode report working, waiting
  and finished with nothing written by Roost under `~/.claude` or
  `~/.config/opencode`, locally and on a host session. A user who said
  `off` gets neither variable; a fresh install gets both with no card;
  under a reproduced sideload policy Claude starts normally in a Roost tab
  because Roost skipped its variable.

### Phase 2: Retire file delivery, integration v4 (L)

- **Depends on** Phase 1. **Ship in the same release.** Phase 2 without
  Phase 1 removes hooks with nothing in their place. Phase 1 without
  Phase 2 doubles every event for every existing user until the retire
  lands (D1 hides only the banners). If Phase 1 must ship first, it ships
  with injection off by default.
- **Scope.**
  - The automatic retire with the strict predicate (`Intent::Retire`,
    [What Roost may remove](#what-roost-may-remove)): the frozen
    per-release table of handler timeouts and opencode plugin images; the
    re-render layout check; backups with `index.json`; the tombstone
    record with leftovers and reasons; the once-per-version marker; the
    read-only `came_back` check at startup.
  - The retire at `roost-session` start on hosts, and the UI's deferral
    while an older session serves this machine.
  - The `agent-hooks` shadow write; `session.set_agent_hooks` per agent
    on a v4 session, including an empty list; the client choosing what to
    send by the host session's version.
  - **Codex and Grok off files, in this phase** (moved from Phase 4:
    with gx dropped and Codex 7501-only decided, nothing keeps either as
    a file agent, and the no-card design has no switch for them in the
    gap). The retire removes Codex's `hooks.json` handlers with their
    canonical trust keys and restores `[features] hooks` from the
    pre-image; it deletes `$GROK_HOME/hooks/roost.json` when `is_ours`
    holds. Depends on the standalone `app=grok` fix.
  - The "Agent integration" card (first run only for a file opt-in, i.e.
    Cursor; preferences with a switch per agent; "Remove…" for
    leftovers; see [The startup
    prompt](#the-startup-prompt-agent-integration)), the palette and Mac
    menu rename, and the one-time toasts.
  - The `roostctl` verb changes; `uninstall` gains the backup and the
    layout check.
  - The docs rewrite, the stale-doc fixes listed under Today, and the new
    DL. Update vision.md's "Agent awareness" line.
- **Phase 0 facts this depends on.** That a `settings.json` Claude
  rewrote re-renders byte for byte through Roost's printer (likely; see
  "How common will `would_reformat` be?"). If not, most Claude users
  would get `would_reformat` leftovers instead of a clean retire, and
  Phase 2 needs a byte-span JSON removal first (+M). And the per-release
  handler timeouts, built from the release tags, not assumed to be 10
  (Codex clamps two events to 3, `codex.rs:57`).
- **Closes.** #485 and #492 for the moved agents; Cursor is the only
  file agent left, and it is off by default, so both can be narrowed to
  Cursor or closed. Shrinks #553 item 2.
- **Size.** Still L; Codex and Grok add acceptance cases, not new
  machinery.
- **Tests.**
  - `crates/roost-agent-install/src/acceptance.rs`, one case per row of
    both tables in [What Roost may remove](#what-roost-may-remove):
    - a group with a `matcher` is left as `annotated`;
    - a changed timeout is left as `edited`;
    - a second exact copy is left as `duplicate`;
    - a compact-formatted file is left untouched as `would_reformat`;
    - an opencode file with the marker and an edited body survives;
    - cursor's flat handler list is retired with no group check;
    - an orphaned trust key with a canonical hash goes;
    - a non-canonical one stays.
  - A fixture home holding v1, v2 and v3 entries plus a user hook in the
    same event comes out with only the user hook, and is byte-identical
    outside the removed spans.
  - `agent-hooks = off` with exact entries on disk: retired, the key
    stays `off`, the "now possible" notice is shown once.
  - Unanswered key with cursor entries on disk: cursor kept until the
    card is answered; Claude, Codex and Grok entries retired.
  - The strict Codex retire across both files (an edited handler keeps
    its non-canonical trust key; an orphaned canonical key goes),
    pre-image restore. A `roost.json` with a user `matcher` is left as
    `foreign`; Roost's exact `roost.json` is deleted.
  - Idempotent: a second run writes nothing, makes no backup, shows no
    toast. A cut after one agent completes on the next run with no
    second backup of the finished agent's files. A lost record re-runs
    to a no-op.
  - Backups are mode 0600, capped at 3 per file, and `index.json` names
    each original.
  - With an older local session reported as serving, the UI does not
    retire; the session's v4 start does.
  - An exact entry put back after the retire is reported as `came_back`
    and not removed.
  - roosttest session lane: a v3-style raise of `["claude"]` against a
    new session answers `skipped: no file needed` and writes nothing in
    the jailed home; an empty list from a v4 client is accepted and
    carries the host's notice.
  - roosttest `test_agent_hooks_dialog.py`, reworked: `app.dialog_dump`
    reports rows with `delivery`; the first-run card appears only with
    both keys absent and Cursor installed, and lists Cursor alone,
    unchecked; with only Claude, Codex and Grok installed there is no
    card; an answered upgrader gets no card; "Decide later" writes
    nothing.
- **Exit.** Upgrading a machine with v3 entries leaves no provably-Roost
  entry in `settings.json`, `plugins/`, `~/.codex` or `~/.grok`, names
  every other one in doctor
  and `agent status`, has a backup of every changed file, and Claude
  still reports. A host is cleaned at its first v4 session start. No
  machine passes through a window where a running session's tabs have
  neither file nor env delivery.

### Phase 3: Process layer and attribution (XL as two plans; parallel with 1–2)

- **Two plans.** 3a, the process layer (identity, stamps, end rules,
  `133;D;$?`, the DL-14 and DL-29 amendments), is L on its own. 3b, peer-pid
  attribution, is M: new socket code on two platforms (`SO_PEERCRED`'s pid
  on Linux, `LOCAL_PEERPID` on macOS), a parent walk on each, and its own
  observe-then-enforce release cycle. 3b does not need 3a's stamps.
- **Scope.**
  - Foreground identity with argv normalisation (node, bun, python
    wrappers).
  - Process stamps on records and owners; exit and suspension rules; the
    1 s tick for live tabs only.
  - `133;D;$?` from bash and zsh; the "exited (code)" banner.
  - Peer-pid session and ancestry checks, observe-only first: count
    `not_in_tab` and nested-by-ancestry in their own counter, apply the
    report anyway.
  - DL-14 and DL-29 amendments; shared fixtures updated.
  - Codex runs hook commands in a new session and its threads in a
    detached daemon (Source; seen live), so 3b's same-session check would
    class every Codex hook `not_in_tab`. Under the recommendation (retired
    in Phase 2) Codex has no hooks, so this matters only if Codex hooks remain.
- **Closes.** #519, #621. #623 and #624 when enforcement flips (this plan
  or the next release).
- **Tests.**
  - `tests/agent-state-fixtures` updated for the reversed label rule.
  - roosttest with `tab.feed_pty_bytes`: a 7501 `working` record, then a
    SIGKILL of the stamped process in a tab running `sh` (no marks): the
    tab leaves `running` within about a second.
  - Ctrl-Z keeps the owner and shows suspended.
  - A report from a `setsid` helper counts as `not_in_tab`.
  - A nested fake `claude -p` under a fake Claude counts as nested.
  - The peer-pid tests run on macOS too (the macOS legs of CI's
    `rust-build` and `iced-build-e2e` jobs), not only Linux.
  - A wrapper that forks the agent and exits (a `node` shim) does not end
    the owner: the stamp follows the process group.
- **Exit.** No stuck "running" after a kill, crash or `/exit` with marks
  off. Would-drop counts per reason are visible in doctor; they are not
  in `Tab.dropped_reports`, which holds only a total and the last drop.

### Phase 4: Cursor, and dropping gx (S–M)

Codex and Grok moved into Phase 2 (2026-11-08). What is left:

- **Depends on** Phase 2's machinery (the strict retire, the card's
  delivery table). The `gx-gate` re-capture with grok (Phase 0) before
  its fixture is deleted.
- **Scope.**
  - **Cursor (decided: file opt-in, unchecked).**
    - Fix the installer to write `$HOME/.cursor/hooks.json` whatever
      `CURSOR_CONFIG_DIR` says (`roost-agent-install/src/home.rs:23`);
      cursor 2026.11.05 reads user hooks only there (Proven). The retire
      and `uninstall` keep looking in the old location through the
      record. Can ship earlier as a standalone fix.
    - Its entries are retired only when the user switches it off.
    - Optional no-write tier: a `cursor-agent()` (and `agent()`, only
      when it resolves to cursor's launcher) shell function adding
      `--plugin-dir <roost cursor dir>`. Gives presence, working and
      session end, but **no turn end**, because cursor gates
      `beforeSubmitPrompt`, `afterAgentResponse` and `stop` on the user
      or project `hooks.json` (Proven). Not recommended as the default;
      worth it only if Charlie wants Cursor "working" with no opt-in.
      Untested: whether `resume`, `ls` and `persist` take a leading
      `--plugin-dir`.
    - Not a route: advertising Ghostty (`GHOSTTY_RESOURCES_DIR`) would
      make cursor send OSC 777 "waiting for you" while unfocused, but
      would make every other program think the terminal is Ghostty.
  - **Drop gx (decided).** Delete, from the grok probe's inventory
    (main @ 68231b9b):
    - `crates/roost-agent/src/grok.rs`: the gx mentions in the module
      docs (lines 1–71), the `gxRemote` → `gx.remote` metadata stamp
      (about 159–187) and its notes at 245, 268, 303–304 and 352. Keep
      `loopback_base_url` in `common.rs` (opencode's `server_url` uses
      it).
    - `crates/roost-agent/tests/grok_events_test.rs:486-596`: the two
      `gx.remote` tests.
    - Fixtures `crates/roost-agent/tests/fixtures/gx.jsonl`,
      `gx-gate.jsonl`, `gx-failure.jsonl`; their names in
      `crates/roost-agent/src/hook.rs:280-282`; the 23 references in
      `crates/roost-agent/tests/fixture_replay_test.rs`, including the
      leader-mode note at 433 (grok-build#14).
    - `crates/roost-cli/src/doctor.rs`: `"gx.remote"` in the metadata
      allowlist (1617), its loopback display (1629), the test at
      5493–5501.
    - The "gx is not its own agent name" pins:
      `crates/roost-agent/src/lib.rs:52,163-176`,
      `crates/roost-agent-install/src/ensure.rs:926-932`,
      `crates/roost-cli/src/agent_install.rs:641-642`, the help text at
      `crates/roost-cli/src/main.rs:455`.
    - Comments: `crates/roost-agent/src/codex.rs:82`,
      `crates/roost-agent/src/common.rs:41`,
      `crates/roost-agent/tests/codex_events_test.rs:212`,
      `crates/roost-ipc/src/agent.rs:546`,
      `crates/roost-agent/assets/opencode/roost-agent-state.js:71`,
      `crates/roost-agent-install/src/grok.rs:1,6`; and
      `crates/roost-engine/src/program_status.rs:603,1132`, which the
      `app=grok` fix already rewrites.
    - Docs: `docs/guides/agents.md` (intro line 5, table row 28, lines
      33, 45, 100–102, 168, the `## gx` section 690–735, the `gx.remote`
      cross-reference at 805, the leader-mode caveat 827–855, "gx shares
      grok's file" 876–879); `docs/reference/config.md:44,484`;
      `docs/reference/cli.md:834`; `docs/reference/ipc.md:1588` (pick a
      new metadata example); `docs/guides/notifications.md:90`;
      `docs/guides/host-sessions.md:260`;
      `docs/getting-started/first-run.md:59`;
      `docs/development/vision.md:796`.
    - Tools: `tools/agent-status/README.md:59` and
      `scripts/gx-startup.json`. `tools/roosttest/test_real_input_mac.py:434`
      names gx only in a docstring; the Shift+Enter test stays.
    - Keep: `CHANGELOG.md`'s 12 historical mentions. `skills/` has none.
  - Whether the grok hook **adapter** itself goes too (grok has no hooks
    after Phase 2) is a one-back question: a leftover `roost.json` the
    retire could not prove still calls `agent-hook grok`. Keep the
    adapter for the compatibility window, then remove it with the
    `grok` agent token's install path.
- **What Codex loses under (a)**, in user terms (accepted by choosing
  it; the retire itself is in Phase 2):
  - an approval reads "Needs your input" rather than "Needs permission to
    use <tool>" until Codex's `msg` ask lands;
  - no row on Codex's sign-in, trust and update screens, and the row
    leaves on Ctrl-Z and returns on `fg`, until Phase 3a;
  - `roostctl doctor` loses the Codex owner, model and session lines (the
    model was never in the UI, and Codex never sent a session title);
  - no exact "Resume Codex" (Phase 7) without a session id, though Codex
    0.167.0 prints `codex resume <id>` on exit.

  What Grok loses without hooks: the session id and ownership (its row is
  the ownerless 7501 kind), `SessionStart.source`, the hooks'
  `idle_prompt` and `agent_error` notification types, StopFailure's error
  class, and the model and permission-mode lines in doctor. It gains
  subagent child records, a more specific blocked `msg` (the tool
  summary, "Trust this folder?", "Allow <subagent>?") and no per-folder
  trust prompt.

  Nested-agent protection and duplicate-notification muting stay (see
  [Codex without hooks](#codex-without-hooks)). Agents run inside tmux,
  screen or zellij in a Roost tab lose state entirely: the multiplexer
  does not pass 7501 through, and Phase 3b drops their hooks once
  enforced. Doctor should say so.
- **Closes.** What remains of #485 and #492, narrowed to Cursor.
- **Tests.**
  - Cursor: the installer writes `$HOME/.cursor/hooks.json` with
    `CURSOR_CONFIG_DIR` set; an entry in the old location is still found
    by `uninstall` and the retire.
  - After the gx deletion, `cargo test` and the fixture replay stay green
    on the re-captured grok `gate` fixture.
  - The existing record-only row tests (`agent_palette.rs:660`,
    `test_program_status.py:587`) stay green; add an `app=codex` and an
    `app=grok` record case that activates through the palette.
  - If the cursor shell function ships: a roosttest that its argv carries
    `--plugin-dir` and that a user's own `cursor-agent` function wins.
- **Exit.** A default install writes nothing into any agent's config, and
  no gx code or doc remains outside `CHANGELOG.md`.

### Phase 5: Presentation (L)

- **Depends on** Phase 0 (children facts) and Phase 3 (stamps for "same
  episode").
- **Scope.** Field fusion; `reason {kind, text, from}` on the wire; kind
  copy and icons in the panel, palette and banners; children on the wire
  (top 8) and as nested rows; done clears on seen; evict the oldest
  record instead of refusing.
- **Closes.** #649.
- **Tests.** Program-status unit tests for fusion and eviction;
  `test_program_status.py` and `test_agent_panel.py` with fed 7501
  records for each kind; a seen-clears test driven by tab focus.
- **Exit.** A Codex approval reads "Needs approval" (with the tool only
  while Codex hooks remain, or once Codex sends `msg`). A Claude
  background task shows as a row, if Phase 0 confirmed child records. A done dot clears when the tab is
  looked at.

### Phase 6: Automation (S–M)

- **Depends on** Phase 5 (`reason`).
- **Scope.** A `turns` counter; `tab prompt` refuses
  `blocked kind=permission` without `--force` and uses `turns` after a
  stream loss; `wait --until blocked:question`; `reason` in verb JSON.
- **Closes.** #516.
- **Tests.** roosttest: prompt into a permission-blocked tab exits
  non-zero without typing; a turn that ran during a dropped stream
  reports done.
- **Exit.** An orchestrating agent can tell why a tab is blocked and
  never types into an approval.

### Phase 7: Resume on restore (M–L)

- **Depends on** Phase 1 (Claude session id).
- **Scope.** Persist `{agent, session_id, cwd, argv}` with the tab
  layout (`crates/roost-engine/src/persistence.rs` has no agent fields
  today); a restore affordance; a per-vendor argv table, each verified.
  No auto-run by default.
- **Closes.** None (backlog-triage item 13, unfiled).
- **Limits.** Needs a session id, so Codex is out under 7501-only (its
  `codex resume --last` filters by cwd, so it is wrong when two Codex
  tabs share a directory; 0.167.0 prints `codex resume <id>` on exit,
  which Roost could read from the screen, Inferred). Grok, also
  7501-only, has the same gap. A
  host-session tab persists in `roost-session`'s state, so the record
  goes there too; a mobile client needs the affordance as an op, not
  only a button.
- **Tests.** Persistence round-trip with and without the agent fields
  (an older `state.json` still loads); roosttest: a restored tab offers
  resume and runs nothing until it is accepted.
- **Exit.** After a reboot or a forced session restart, a restored tab
  offers "Resume Claude · <title>".

### Phase 8: Hints (S)

- **Depends on** Phase 3 (a stamp for hint ownership).
- **Scope.** libghostty's `PROGRESS_REPORT` callback as a working hint;
  bell as attention for tabs with no other signal.
- **Closes.** #364.
- **Tests.** roosttest with `tab.feed_pty_bytes`: `OSC 9;4;1` moves an
  otherwise idle tab to `running`, `9;4;0` ends it, `9;4;2` does not
  project Failed; a bell in a tab with a live record adds nothing.
- **Exit.** A tab whose agent sends only 9;4 progress shows Working, and
  stops when the agent clears it or exits.

### Track, not plan

- The upstream asks below.
- #508 (wait on a host tab; needs the session stream).
- #560 (window ops from remote-host tabs; the outbound control surface,
  not hooks).
- #509 (skill packaging for Codex and opencode). The Claude skill
  already ships as the `roost` plugin, and (decided) rides in the env
  plugin inside Roost tabs. The opencode env route could carry
  the skill too, which is #509's opencode half.

### Order

This is the main feature track. Backlog Plan A (v0.1.1 quick wins, in
`backlog-triage-2026-11.md`) ships first; Phases 1–2 can then run beside
backlog Plan C.

0. Now, standalone: the `app=grok` fix (and, if convenient, Cursor's
   `$HOME/.cursor/hooks.json` path fix). Both fit backlog Plan A.
1. Phase 0's remaining delivery fact (Claude with auth) first or as
   Phase 1's first commit; the sideload policy is already proven. Its
   7501 captures only gate Phase 5. Then Phase 1 and Phase 2 together,
   in one release; Phase 2 now retires Codex and Grok as well.
2. Phase 3a in parallel. It is needed for spec compliance whatever is
   decided about hooks. Phase 3b after, with its observe-only release.
3. Phase 4 (Cursor and the gx deletion) after Phase 2, once grok's gate
   fixture is re-captured.
4. Phase 5, then Phase 6.
5. Phases 7 and 8 whenever there is room.

---

## Upstream asks

| Ask | Project (license) | Why |
|---|---|---|
| Write the existing activity and approval detail as 7501 `msg`, not only into OSC 21337 | openai/codex (Apache-2.0) | Codex blocks have no reason. Removes the last reason to keep Codex hooks. Small change in `terminal_program_status.rs` |
| Send the session name as the root `title`, and a session id | Claude Code (closed) | Ownership and resume without hooks |
| A `session=` key for an opaque resumable session id | OSC 7501 spec (Superlogical) | Lets 7501 carry DL-14 identity and resume for every agent. Unknown keys are ignored, so it degrades safely |
| Release 7501 to npm `latest` | opencode (MIT) | Already in v2.2.6. Track only |
| 7501 | cursor-agent (closed) | No blocked signal at all |
| Run `beforeSubmitPrompt`, `afterAgentResponse` and `stop` for plugin hooks too, not only user and project `hooks.json` | cursor-agent (closed) | Would let `--plugin-dir` carry full state with no file, ending Cursor's opt-in |
| Read user hooks from `CURSOR_CONFIG_DIR` as it does config and auth, or document `$HOME/.cursor` | cursor-agent (closed) | Today the variable moves everything but hooks |
| Expose OSC 133 `aid` in the semantic-prompt event | Ghostty (MIT) | DL-29 rule (c): tell an agent's marks from the shell's |
| Gate 9;4 and 7501 on the 7501 probe, not a `TERM_PROGRAM` list | Claude Code, Codex | A probe is the honest capability check |

---

## Risks

- **Claude refuses to start under a sideload policy.** On a machine with
  managed `disableSideloadFlags`, every Claude in a Roost tab would exit 1
  (Proven). Mitigations (decided, A+C): the `claude plugin list` probe
  skips injection, a doctor line, a one-time toast, the per-agent off
  switch. Residual: a server-managed policy that no Claude run has cached
  yet (Inferred), and a `claude` the probe cannot find on the serving
  process's PATH. Claude's own message then names the variable to unset.
  No `--settings` fallback: it would get around the org's ban.
- **Claude's richer 7501 is read from strings only.** Phase 5 must not
  design rows before Phase 0 captures it.
- **Duplicates during migration.** Proven to happen. D1 makes the user
  impact one banner, not two.
- **The user's rc can replace Roost's env value.** Mitigated by the
  integration re-append; not covered for fish, command tabs or Apple bash
  3.2.
- **Version skew.** An older Roost elsewhere can put file entries back.
  The startup check reports them as `came_back`; they are not chased.
- **An older session outlives the UI upgrade.** Retiring under it would
  leave its tabs with no hooks. The retire defers to that session's own
  v4 start.
- **Inherited plugin variables.** A session or UI started from a Roost tab
  inherits another process's Roost folder. Strip before appending; a
  constant plugin name makes a miss harmless (Proven: same-name plugins
  load once).
- **An explicit `off` ignored.** If env injection read a new key with no
  carry-over, every user who answered `off` would get hooks again. Phase
  1 carries `agent-hooks` into `agent-integration` once, before any tab
  is spawned with the variables.
- **Strict removal leaves leftovers that still fire.** An edited or
  annotated Roost entry stays, and runs beside the env plugin until the
  user removes it. D1 covers the banners; doctor and the card name it
  with a "Remove…".
- **A wrong "provably ours" table.** If the frozen per-release table of
  handler shapes misses a release, its exact entries become leftovers
  (safe, but noisy). Build it from the tags, not from memory.
- **Agents inside a multiplexer.** 7501 does not cross tmux, and Phase 3b
  drops hooks from outside the tab's session. An agent in tmux inside a
  Roost tab works today and would show nothing after Phases 3b and 4.
- **Claude older than v2.1.280** ignores the variable. Once its file
  entries are retired it has no hooks, and 7501 only if it is new enough
  to speak it. **Accepted** (decided, key question 9): Claude updates
  often, and Roost does not support it below the floor. Doctor can name
  an old `claude` on PATH.
- **Reversing DL-14's label rule** changes the shared state fixtures. With
  Swift retired (DL-33) there is one port to keep in step.
- **False release.** A wrapper that execs or forks and exits can look like
  an exit. Follow the process group, not only the leader pid. herdr's tests
  are a checklist.
- **False `not_in_tab`.** A legitimate hook process outside the tab's
  session (tmux panes, an agent that runs hooks under `setsid`).
  Observe-only first.
- **A 1 s tick in `roost-session`** with many tabs. Bounded to tabs with
  live state; one stat per tab.
- **"Seen" over several clients.** Simplest rule: any client viewing the
  tab counts, the phone included.
- **grok's `app` changed under Roost** (`grok-build` to `grok` by
  1.0.77). Today's hooks fail silently for grok users until the fix
  ships. Agents rename `app` without notice; a doctor line for a record
  `app` Roost has no label for would catch the next one.
- **Detached agent processes.** Codex 0.167.0's app-server daemon and
  cursor's `worker-server` outlive the TUI, in their own session with no
  tty. The process layer must not read them as the agent still running.
- **The env plugin shares the marketplace plugin's name.** Inside a Roost
  tab Roost's `roost` replaces the user's installed `roost@roost`, so the
  two SKILL.md copies must stay identical, and a managed lock on a plugin
  named `roost` drops Roost's hooks with only a warning (Source).
- **Codex trust coupling** if the shell function is chosen. A formula
  change upstream means a review dialog, and "Trust all" writes the user's
  `config.toml`.

---

## Key questions for Charlie

Numbers are kept stable, because the text above cites them. Answered
ones say **Decided** and give the answer.

1. **Default. Decided 2026-11-08, revised the same evening:** env and
   7501 agents are simply on, with a per-agent off switch in
   preferences, config and `roostctl`. The startup card ("Agent
   integration") appears only when an installed agent needs a user file
   changed and has not been answered (today only Cursor), and then asks
   only that. Upgraders who already answered are not asked again and get
   a one-time toast, never a modal. See [The startup
   prompt](#the-startup-prompt-agent-integration); the copy is question
   23.
2. **Retire. Decided 2026-11-08:** automatic, once Roost's entries are no
   longer needed, removing only what Roost can prove it wrote, with
   backups. Everything else is left and named in doctor, `agent status`
   and the card, with a "Remove…" button.
3. **Legacy key. Follows from 2:** `agent-integration` is the only key a
   new build reads; `agent-hooks` becomes a narrowed shadow so an older
   Roost on the same machine never puts back what the retire removed.
4. **Hosts. Follows from 2:** a new `roost-session` cleans its own
   machine at start; a client never edits a host's files.
5. **Codex.** Codex without hooks already meets your bar: its panel and
   palette rows update, show status, and take you to the tab (proven
   live). You lose the tool name on approval banners ("Needs your input"
   instead of "Needs permission to use Bash") until Codex sends it, and,
   until Phase 3a, the row on Codex's sign-in screen and while suspended.
   **Recommended: go 7501-only, no shell function.** See [Codex without
   hooks](#codex-without-hooks). **Decided (Charlie, 2026-11-08):** Codex
   goes 7501-only. His bar was that the Agents panel and the agent palette
   update, show status and are clickable, which a hook-free Codex meets. The roadmap now retires Codex's entries
   in Phase 2. 0.167.0 re-confirmed the 7501 stream.
6. **Grok and gx. Decided 2026-11-08:** drop gx support entirely. Grok
   goes 7501-only with no Roost file; `~/.grok/hooks/roost.json` is
   retired by the proven-ownership retire (Phase 2), and the gx code,
   docs and tests are deleted (Phase 4). No 7501 in the fork, no gx
   upstream ask.
7. **cursor. Decided 2026-11-08:** file delivery stays an explicit
   opt-in, **unchecked** by default, and the card shows only when Cursor
   is installed. `--plugin-dir` is now proven but partial (no turn end),
   so it does not move Cursor into the no-writes group; a shell-function
   tier is optional (Phase 4).
8. **Org policy blocks Claude plugins. Decided 2026-11-08: A+C.** Detect
   the policy with the `claude plugin list --json` probe and skip Claude's
   variable, plus a doctor line and the per-agent off switch. No shell
   function and no `--settings` fallback. The probe catches file and MDM
   policy (Proven on Linux files); a server-managed policy only once
   cached (Inferred).
9. **Old Claude (below 2.1.280). Decided 2026-11-08:** not supported.
   No `--file` fallback. The marketplace or plugin-install route may be
   revisited if env delivery fails in practice; not pursued now.
10. **Duplicate banners.** May Roost skip a banner identical to one
    already pending on the tab, everywhere, not only during the
    migration?
11. **Which signal wins.** When the terminal report (7501) and a hook
    disagree about a tab, the terminal report wins. Is there any case
    where a hook should win?
12. **When an agent dies, should its row go away?** **Decided (Charlie,
    2026-11-08): yes.** Once the process table shows the agent's process
    is no longer running, ownership ends and the row leaves the Agents
    panel and palette; the tab stops muting notifications. A suspended
    agent (Ctrl-Z) keeps its row, because its process still exists. This
    amends DL-14's "label survives" rule when Phase 3a lands. See [Ownership after an agent
    dies](#ownership-after-an-agent-dies).
13. **Done until seen.** Should a finished or failed dot clear when you
    look at the tab, instead of (or as well as) when the next command
    starts?
14. **Hooks from outside the tab.** Roost can tell a hook sent from inside
    the tab from one sent by a background daemon or a tmux pane. Count
    such hooks for one release before dropping them? Dropping them means
    an agent inside tmux in a Roost tab shows nothing.
15. **Skill. Decided 2026-11-08:** yes, the Roost skill rides in the
    env plugin. The plugin is named `roost`, so inside a Roost tab it
    replaces an installed marketplace `roost@roost` and the skill loads
    once (Proven). The marketplace plugin stays for Claude outside Roost
    tabs, with an identical SKILL.md. (#509 is the Codex and opencode
    half.)
16. **Background tasks and `tab prompt`.** Show an agent's background
    tasks as nested rows (top 8) or a badge? Should `tab prompt` refuse a
    tab waiting on a permission prompt by default, but allow a question?
17. **Resume.** Offer "Resume Claude" with one click after a restart, or
    run it automatically with an opt-out?
18. **Upstream.** Who files the Codex `msg` ask and the 7501 `session`
    key ask, and through which channel for the spec?
19. **Host deferral. Follows from 2:** while an older `roost-session`
    still serves a machine, the retire waits for that session's restart,
    so its tabs never lose hooks.
20. **Host "no".** If an older Roost client connects to a host whose user
    said no, may it switch agent integration on there? Recommended: no
    for Claude and opencode (today's raise widens over `off`).
21. **"Decide later".** Recommended: on the Cursor card it writes
    nothing and the card returns next launch. (Env and 7501 agents are on
    by default either way, per question 1.)
22. **Entries that come back** after the retire (a synced dotfile, an
    older Roost elsewhere). Recommended: report them with a "Remove…",
    don't remove again each launch (that would loop with the older
    Roost). Agree?
23. **Card copy and mockups** in [The startup
    prompt](#the-startup-prompt-agent-integration), redrawn for the
    Cursor-only first-run card and the preferences card: OK as drafted?
    Also: should the off switch for a built-in (7501) agent exist, and
    mean "ignore its records"?

---

## Review (2026-11-08)

An adversarial pass over this note, the same day. It re-ran the riskiest
claims live and read the migration against the code.

- **Held, re-proven:** Claude's env plugin in the interactive TUI (b1);
  Codex's computed-hash `-c` route with no bypass (a1, a2).
- **New facts:** a two-folder `CLAUDE_CODE_PLUGIN_DIRS` loads both (b1);
  same-named plugins load once, first wins (b2); Codex's `-c hooks.state`
  merges with the user's own trust entries in `hooks.json` and
  `config.toml` (a0–a4).
- **Fixed in the design:** consent carry-over from `agent-hooks = off`
  moved into Phase 1; the retire defers while an older `roost-session`
  serves the machine; inherited Roost entries are stripped before
  appending; a host's retire notice needs a connect-time raise; the
  "New UI, old session" row no longer has the new client writing Claude
  into hosts; the kept-record rule is back in the precedence; observe-only
  counts get their own counter; Phase 1 and 2 must share a release.
- **Fixed in the text:** phase sizes and tests, #509, the SKILL.md claim,
  the crush and pi coverage claim, Phase 5's Codex exit.
- **Left for Charlie:** key questions 9, 14, 19 and 20 carry the new
  judgment calls.

### Second pass: Charlie's answers

Later on 2026-11-08 Charlie answered the first round:

1. **Retire:** "If no longer needed roost should remove entries from the
   files automatically, and we will also want to update the prompt that
   we popup on startup asking what integrations we should turn on. We
   will want to avoid changes that we aren't confident were made by
   roost." Now decided: [Principles](#principles), [What Roost may
   remove](#what-roost-may-remove), [The startup
   prompt](#the-startup-prompt-agent-integration).
2. **Codex:** unsure; wanted discovery. Acceptable without hooks as long
   as the Agents panel and the palette update properly, show the status,
   and the rows are clickable. Answered in [Codex without
   hooks](#codex-without-hooks): it already does.
3. **Ownership:** the question was unclear. Rewritten in user terms in
   [Ownership after an agent dies](#ownership-after-an-agent-dies), with a
   recommendation.

What this pass did, and what it checked:

- **Held, re-checked in code:** the row rule (`row_agent`), the click
  path (`focus_tab_and_clear` from both panel and palette), the
  `ForeignApp` veto, `suppress_raw_osc` muting, the record-only row tests,
  Codex's 7501 writer (no `msg`, `title`, `id`) and its hook runner's new
  session, `remove_grouped`'s annotated-group rule, `is_ours` for grok and
  opencode, `our_hashes`, the free-string `skipped` reason, DL-36 as the
  next free number.
- **Live (Proven):** a real Codex 0.165.0 with no hooks in an isolated
  dev Roost: rows appear, update, banner on failure, palette activation
  moves focus; a kill under bash leaves nothing stale; a kill under
  `/bin/sh` sticks "Working" and mutes a later raw OSC 9. A fake hooked
  agent showed the stale "Idle" owner after a kill under bash.
- **Corrected while folding in:**
  - Cursor's `hooks.json` is flat (no groups), so the strict predicate's
    group clause applies to Claude and Codex only.
  - Model is shown nowhere in the UI, and Codex never sent a session
    title, so "Codex loses model and title in the Agents panel" was
    wrong.
  - Codex's acknowledge lives in `tui/src/chatwidget/status_surfaces.rs:341`
    (called from `tui/src/app.rs:926`).
  - The UI startup hook is `window_opened` at `app.rs:4415`.
- **New fact:** this box's `~/.claude/settings.json`,
  `~/.codex/hooks.json` and `~/.cursor/hooks.json` are all in the
  two-space layout Roost's printer reproduces (read-only check, Python as
  a stand-in for the printer), so `would_reformat` should be rare.
- **Incident during the live Codex probe.** A second `codex` launch in
  the isolated tab showed Codex's "update available" prompt; the Enter
  meant for a prompt picked "Update now", and Codex ran `npm install -g
  @openai/codex`, upgrading the user's real global Codex from 0.165.0 to
  0.167.0 (under the mise node 24.20.0 prefix). `~/.codex` was not
  touched. The install ran under umask 0077, so the new files are mode
  0700/0600, and npm's mise reshim step failed ("mise: command not
  found"); `codex --version` reports 0.167.0. Not reverted: rollback is
  `npm install -g @openai/codex@0.165.0`; if 0.167.0 stays and the modes
  matter, `chmod -R go+rX` on that package dir. Phase 0 now carries the
  probe-hygiene rule.

### Third pass: Charlie's second answers and Phase 0 probes

**Charlie's second round, 2026-11-08 (decided):**

1. **gx:** "we can drop the gx support entirely if grok itself can be
   updated to the new method." grok 1.0.77 can (below), so gx support is
   dropped: Grok goes 7501-only and `roost.json` is retired. Key question
   6.
2. **Cursor:** "ok to leave it unchecked by default, but we likely only
   need to show the card if it is installed". Key question 7.
3. **Startup card:** "if the rest don't need changing the users files
   likely we don't need to pop that up at the beginning". No card unless
   a file opt-in exists for an installed agent; upgraders get a toast.
   Key question 1 and [The startup
   prompt](#the-startup-prompt-agent-integration).
4. **Claude:** the env plugin is fine if additive to the user's own
   plugins (it is, Proven below); policy option A+C; "we don't need to
   support older versions at this time … we can fall back to the other
   plugin install option in the future but I don't want to pursue this
   now." Key questions 8 and 9.
5. **Skill:** "including the roost skill also makes sense." Key question
   15.

Codex 7501-only was later confirmed by Charlie (key question 5).

**Phase 0 probes, same evening.** Charlie had just updated the agents:
claude 2.1.319, codex-cli 0.167.0, cursor-agent 2026.11.05-86047df (now
logged in), opencode 1.19.5, grok 1.0.77, crush 0.97.1. Each probe ran
under `env -i` with a temp `HOME`, XDG dirs, `TMPDIR` and the agent's
home variable under the scratchpad `p0/<agent>/`, no `ROOST_*`, `CLAUDE*`,
`CODEX_*` or `HERDR_*`, and every updater off. Only login files were
copied in (cursor, grok, Codex), never refreshed, then deleted. sha256 of
the real config dirs before and after were identical, except two files
written by Charlie's own running gx leader (pid 2806985), which the
probes did not touch. No update, login or trust prompt was accepted;
probe-started Codex daemons and a cursor `worker-server` were killed.

| Agent | Result | Status |
|---|---|---|
| grok 1.0.77 | 7501 alone gives the states, kinds and messages Roost shows, plus subagent child records; `app=grok` | Proven captures; row from code (Inferred) |
| grok 1.0.77 | Today's `roost.json` hooks are dropped as `ForeignApp` under 1.0.77, and the row label is `grok` | Inferred from code, high confidence |
| grok 1.0.77 | Runs `~/.claude/settings.json` hooks; does not run Roost's flat `~/.cursor/hooks.json` entries; any hook file causes a per-folder trust prompt | Proven |
| cursor 2026.11.05 | `--plugin-dir` delivers start, end, thought and tool events, never `beforeSubmitPrompt`, `afterAgentResponse` or `stop` | Proven; gate read in the bundle |
| cursor 2026.11.05 | User hooks read only from `$HOME/.cursor/hooks.json`, not `CURSOR_CONFIG_DIR` | Proven |
| cursor 2026.11.05 | No 7501, no 9;4; OSC 777/9/99 only for terminals it recognises, none for `TERM_PROGRAM=Roost`; a detached `worker-server` outlives it | Proven |
| Claude 2.1.319 | Env plugin additive to installed and env user plugins; `settings.json` untouched | Proven |
| Claude 2.1.319 | Env plugin named `roost` replaces the installed marketplace `roost@roost`; one skill, the env copy's text | Proven |
| Claude 2.1.319 | `disableSideloadFlags` exits 1 for `-p`, TUI and `plugin list`; `plugin list --json` detects it in about 0.1 s; `--version` does not; `--settings` is not blocked | Proven (rootless namespace on Linux) |
| Claude 2.1.319 | The probe also sees a server-managed policy once cached | Inferred |
| Claude 2.1.319 | StopFailure under a `-p` auth failure | Failed: unreliable |
| Codex 0.167.0 | 7501 states unchanged from 0.165.0 (incl. `error` with no msg, Esc gives idle) | Proven |
| Codex 0.167.0 | A managed app-server daemon outlives the TUI; exit prints `codex resume <id>` | Proven |
| opencode 1.19.5 | No 7501 through a real turn; the `OPENCODE_CONFIG_CONTENT` plugin route still works beside a user plugin | Proven |
| crush 0.97.1 (apt) | No 7501 at all | Proven |

**What changed in this note:** the Summary, the delivery and signals
matrices, the Claude caveats (2, 8, 9 rewritten; 10 and 11 new), the end
state, consent defaults, the startup card and mockups, rollback, the
roadmap (a standalone `app=grok` fix; Codex and Grok retired in Phase 2;
Phase 4 is Cursor and the gx deletion), upstream asks, risks and the key
questions. Not re-checked by these probes: Claude with a real login, the
parent chains of hook processes, pi.

**Left for Charlie:** 23 (the
redrawn card and a built-in agent's off switch), the optional Cursor
shell-function tier, and whether to stop the gx leader still running
from `~/.grok`.

## Corrections to earlier notes and reports

- **Claude strips `CLAUDE_CODE_PLUGIN_DIRS` from a settings `env` block:
  refuted.** It was honoured (c7). The variable can still come only from
  outside the user's config for Roost's purposes, because Roost will not
  write the settings file.
- **Codex's trust-hash `-c` route was "source only": now proven** in
  `exec` and the TUI, with a negative control. It must be one inline
  table; the dotted form is ignored. `features.hooks` is not needed on
  0.165.0.
- **Claude's interactive TUI with env delivery: now proven** (c2).
- **New: `disableSideloadFlags` makes Claude exit 1** when the variable is
  set (Docs at first; **reproduced** in the third pass, rootless on
  Linux, with a working detection probe).
- **Grok:** `--plugin-dir` is on `grok agent` only and is ignored in
  leader mode, so the TUI has no route (moot now: Grok needs no hooks).
  Grok reads `~/.cursor/hooks.json` as well as Claude's file, but
  **does not run Roost's flat entries** in it (third pass, Proven on
  1.0.77), so "a grok event runs Roost's cursor entries" was wrong.
- **crush:** the installed binary is 0.97.1 (apt). The 2026-10-28 capture
  used the 0.98.2 tarball. Both are right; the third pass's "not 0.98.2"
  is only about what is installed now.
- **pi:** now 1.5.0, and `-e` is a proven argv route.
- **opencode:** npm `latest` is 1.19.5 (now installed too); 7501 first
  ships in tag v2.2.6. The earlier note's "2.2.x source" stands.
- **"Retire the raise op" (third pass): no.** Keep
  `session.set_agent_hooks` on the wire for one-back compatibility and
  reinterpret it per agent. Remove it only after the compatibility window.
- **"A new `DropReason` breaks a closed enum and needs protocol 8" (first
  pass): overstated.** `Tab.dropped_reports` decodes leniently, so an older
  client reads it as absent. No bump.
- **The state record path ignores `ROOST_CONFIG`**, and also
  `XDG_CONFIG_HOME` (`home.rs:159-166`).
- **Claude 2.1.319 child records, kinds, progress: unconfirmed.** Strings
  only, and a spot check did not find the report builder. Phase 0.
- **"7501 covers the states Roost shows for crush and pi" (first draft of
  this note): too strong.** The captures show no `blocked` from crush and
  only `idle`/`clear` from pi.
- **`skills/roost/SKILL.md` "never reaches running" as stale (first
  pass): wrong.** It already exempts agents that report directly.
- **#509 as "the skill inside the Claude plugin" (first draft): wrong
  issue.** #509 is skill packaging for Codex and opencode; the Claude
  skill already ships as the `roost` plugin.
- **Observe-only counts "visible in `Tab.dropped_reports`" (first
  draft): no.** That field holds one total and the last drop.
- **Phase sizes (first draft):** Phases 0, 1, 2, 3 and 5 were each one
  step too small on `backlog-triage-2026-11.md`'s scale.
- **"Env delivery writes nothing, so it needs no first-run card" (first
  draft): reversed, then mostly restored.** The second pass kept a card
  and connected nothing until answered. Charlie's second round
  (2026-11-08): no card unless a user file would change (today Cursor);
  env and 7501 agents are on by default, the sideload probe covers the
  policy risk, and each agent has an off switch.
- **"The retire runs exact-match uninstall" (first draft): tightened.**
  Today's uninstall removes any owned command in any shape. The automatic
  retire now also requires the exact shape a release wrote, an
  unannotated group and no reformat; explicit removal keeps the looser
  rule.
- **"Codex loses model and title in the Agents panel under 7501-only"
  (first draft): wrong.** The model is in no UI surface (doctor only),
  and Codex's adapter never sends a session title.
- **"Without a session id Codex loses DL-14 ownership protection" (first
  draft): overstated.** While Codex's record decides, a different agent's
  claim is dropped as `ForeignApp`, even at Idle, which is stronger than
  the hook rule.
- **New: Codex runs hook commands in a new session** (Source), so a
  peer-session check (Phase 3b) would drop every Codex hook.
- **New: Codex sends no 7501 on its sign-in, trust and update screens**
  (Proven), and holds `done`/`error` until a key (Source).
- **"No child records from the installed agents" (2026-10-28 capture):
  out of date.** grok 1.0.77 sends child records for subagents (third
  pass), so the highest-ranked-record rule now matters in practice.
- **"Grok's 7501 `app` is `grok-build`" (2026-10-28): changed upstream.**
  grok 1.0.77 sends `app=grok`; Roost's mapping is stale (Summary).
- **"The env plugin's name should differ from the skill plugin's
  `roost`" (review, caveat 9): reversed.** Sharing the name is what lets
  the env copy replace the marketplace one with no double load (third
  pass, Proven).
- **cursor `--plugin-dir` "may need `enableUserLocalPlugins`" (Strings):
  no.** It is a runtime capability, true when signed in.
- **Not checked by anyone:** whether agents run hooks in the tab's session
  (attribution; Codex now answered: a new session), whether the Bash
  tool's environment keeps `CLAUDE_CODE_PLUGIN_DIRS`. cursor's
  `--plugin-dir` with auth is now checked (third pass).

---

## References and licenses

**Roost** (main @ 68231b9b):

- `crates/roost-agent-install/src/{command,ensure,state,home,plan,write,jsonedit,lib,claude,codex,codex_hash,grok,cursor,opencode,acceptance}.rs`
- `crates/roost-agent/src/{claude,codex,grok,cursor,opencode}.rs`,
  `crates/roost-agent/assets/opencode/roost-agent-state.js`,
  `crates/roost-agent/tests/`
- `crates/roost-ipc/src/{agent,program_status,messages,peer}.rs`
- `crates/roost-engine/src/{pty,process,program_status,ipc,workspace,persistence}.rs`,
  `crates/roost-engine/resources/shell-integration/{roost.bash,roost.zsh}`
- `crates/roost-session/src/{agent_hooks,agent_hook,serve}.rs`
- `crates/roost-iced/src/{app.rs,app/agent_hooks.rs,app/agent_hooks_dialog.rs,app/agents_panel.rs,app/palettes.rs,host_conn.rs,macos/menu.rs}`
- `crates/roost-cli/src/{main,agent_install,doctor}.rs`
- `crates/roost-ui-model/src/{config,agent_palette,palette}.rs`
- `tools/agent-status/`, `tools/roosttest/{agent_jail,test_agent_hooks,test_agent_hooks_dialog,test_program_status,test_agent_panel}.py`,
  `tests/agent-state-fixtures/`
- `docs/guides/agents.md`, `docs/reference/{ipc,ipc-compatibility,config,cli}.md`
- Decision log (`docs/development/decision-history.md`): DL-7, DL-13,
  DL-14, DL-29, DL-30, DL-32, DL-33.
- Issues, all open, checked with `gh` 2026-11-08: #364, #485, #492, #508,
  #509, #516, #519, #553, #560, #621, #622, #623, #624, #649.

**Third party:**

- Ghostty, MIT. Roost pin `6ba14415`; checkout `eb895f612`
  (`include/ghostty/vt/terminal.h`).
- OSC 7501 spec, Superlogical, rev 0.4 (2026-10-10).
- herdr, Apache-2.0, `ee5bfb5`: `src/terminal/state.rs`,
  `src/detect/mod.rs`, `src/agent_resume.rs`, website docs. Ideas only;
  nothing copied.
- cmux, GPL-3.0-or-later, `a79cbde73ba`: docs only, design level. No code
  read, quoted or paraphrased.
- zellij, MIT, `43c8710`: `zellij-server/src/attention/`.
- Codex, Apache-2.0, `4e4767ffa7`: `codex-rs/tui/src/terminal_program_status.rs`,
  `tui/src/hooks_rpc.rs`, `tui/src/chatwidget/status_surfaces.rs`,
  `tui/src/app.rs`, `hooks/src/engine/command_runner.rs`,
  `cli/src/main.rs`. Probed on codex-cli 0.165.0; 7501 re-captured on
  0.167.0, the version now installed (see [Second
  pass](#second-pass-charlies-answers) for how it got there).
- grok-build, Apache-2.0, `2bdd1d6a`: `xai-grok-config/src/compat.rs`,
  `xai-grok-hooks/src/discovery.rs`, `xai-grok-pager-bin/src/main.rs`.
  Probed grok 1.0.70 and gx 1.0.45+gx.17 earlier; grok 1.0.77 (binary
  `~/.grok/downloads/grok-1.0.77-linux-x86_64`, its embedded docs and
  strings) in the third pass.
- opencode, MIT, `1284450a03`: `packages/tui/src/context/program-status.ts`
  (from `d0a2ce32de`, first in v2.2.6). Probed 1.19.4, then 1.19.5.
- Agent Client Protocol, Apache-2.0, `7441d10`.
- pi, MIT, 1.5.0 via npx.
- crush, FSL-1.1-MIT: docs and design only; the apt 0.97.1 run as a
  black box for its terminal output.
- Claude Code, closed source: 2.1.319 run in isolation (including under a
  reproduced managed policy); binary strings, Linux and (read-only, over
  ssh) the mac-mini's;
  docs at code.claude.com (env-vars, settings reference, plugins CLI
  reference, hooks), fetched 2026-11-08.
- cursor-agent, closed source: 2026.10.22 help and bundle strings;
  2026.11.05-86047df run logged in, and its bundle read for the hook
  gates, terminal detection and update check.

**Probe evidence** (this session's scratchpad,
`/tmp/claude-1000/-home-charliek-projects-roost/722ee52e-3f4c-49dd-ac65-84b1c809c9a5/scratchpad/`):

- `r2/`: Claude `c1`–`c7`, Codex `x1`–`x7` (with `codex-args2.txt` and
  `bin/codexhash.py`), cursor `k1`, opencode `o1`–`o2`, grok `g1`, pi
  `p1`, 7501 captures in `as/out/`.
- `dedupe/`: the identical-command duplicate probe and Claude's own
  `pluginUsage` and `plugins/data` writes.
- `rev/` (the review): Codex `a0`–`a4` (user hooks beside Roost's `-c`
  hooks, with `roost-args.txt`), Claude `b1` (interactive TUI, two-folder
  value) and `b2` (two same-named plugins). Fake API keys, as in `r2/`.
- `cx/` (second pass): the isolated dev-Roost run with a real Codex and
  no hooks, and the fake hooked agent for the dead-owner case (`run.sh`,
  `rc.sh`, `tabs.sh`, `events.jsonl`, `ui.log`).
- `dl/`: the fetched Claude docs.
- `claude-strings.txt`: the 2.1.319 strings extract.
- `p0/` (third pass): `grok/` (captures in `g*/out/`, launcher `mk.sh`,
  credential log, real-config hashes), `cursor/` (hook logs per run,
  captures in `cap/`, probe plugins, hashes), `claude/` (fixtures, the
  `iso` wrapper, the policy runs in `ns/`, `log/hooks.log`, hashes),
  `iso.sh` and `out/` (Codex, opencode and crush captures).
- Earlier: `verify2/`, `verify-oc/`, `verify-codex/`, `verify-codex2/`,
  `grok-inspect.txt`.
