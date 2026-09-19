# Agent Hooks

Roost drives the tab dot, the sidebar rollup, and the desktop banner for
five coding agents — Claude Code, Codex, OpenCode, grok (and its fork
gx), and cursor-agent — by wiring one hook entry per lifecycle event into
each agent's own configuration file. A sixth, craze, needs no wiring at
all: it reports its own state over the socket, which any agent can do
([Reporting directly, without a hook](#reporting-directly-without-a-hook)). Roost **asks before it wires
anything**: the first time the UI starts with no answer on file and at
least one supported agent installed, a consent dialog opens naming
what it found, and nothing is written into any agent's config until you
choose. It works the same way on a remote host session, with one
difference — see [Remote hosts](#remote-hosts).

## Supported agents

Every agent reports through the same four-axis model — ownership,
lifecycle, attention, and free-form metadata — over one wire op
(`tab.agent_report`, [`ipc.md`](../reference/ipc.md#tabagent_report)).
What differs is where each signal comes from:

| Agent | `source` | Config file Roost writes into | Blocked signal | Turn-end signal | Interrupt signal |
|---|---|---|---|---|---|
| Claude Code | `claude` | `~/.claude/settings.json` (or `$CLAUDE_CONFIG_DIR`) — merged in beside your own hooks | `PermissionRequest` (immediate); the `Notification` types `agent_needs_input` and `elicitation_dialog` also set it, unconditionally | `Stop` | none — the post-turn `idle_prompt` notification is the only later signal, guarded so it can't overwrite a real `waiting`/`failed` |
| Codex | `codex` | `~/.codex/hooks.json` + `[hooks.state]` in `config.toml` (or `$CODEX_HOME`) | `PermissionRequest` | `Stop` | `Interrupt` |
| grok / gx | `grok` | `$GROK_HOME/hooks/roost.json` (default `~/.grok`) — a file Roost owns outright | `Notification` `notificationType: permission_prompt` | `Stop` on its first fire; a fire with `stopHookActive: true` (a blocking Stop gate already continued the turn) keeps `working` instead, and the `idle_prompt` Notification settles it | `StopCancelled` |
| cursor-agent | `cursor` | `~/.cursor/hooks.json` (or `$CURSOR_CONFIG_DIR`) — merged in beside your own hooks | none — an accepted gap, see [Per-agent caveats](#per-agent-caveats) | `stop` | `stop` (same event as turn-end; see caveats) |
| OpenCode | `opencode` | `~/.config/opencode/plugins/roost-agent-state.js` (or `$OPENCODE_CONFIG_DIR`) — a plugin Roost owns outright, not a command hook | `permission.asked` / `question.asked` | `session.idle` | `session.error` |
| craze | `craze` | none — craze reports directly, no hook install | a permission, question or plan card is open | turn end → `finished`, banner body "Turn complete" | Esc → `finished` with attention cleared |

grok/gx's `StopFailure` maps to `failed`, its banner body carrying gx's
`errorDetails` (or `error_details`) verbatim rather than just the
classified error name.

Claude and cursor also load hooks from `~/.claude/settings.json`
themselves (grok can be configured to, and cursor always does via its
`claudeUserHooks`), so both adapters positively reject any payload that
carries the other's telltale fields rather than relying on which file it
came from — see [Per-agent caveats](#per-agent-caveats).

[craze](https://github.com/charliek/craze) is the first entry that needs
no hook install at all: it is the process driving the agent over ACP
(cursor-agent, grok, or gx), so it calls `tab.agent_report` directly on
every state change rather than being invoked as a hook. Behavior worth a
sentence beside its row:

- Ownership is keyed by the ACP session id. The first report of a
  session claims the tab with `inactive` and metadata `model`,
  `craze.provider` and `version`; exit releases it.
- An errored turn reports `failed` with the error's first line as the
  banner body.
- craze removes `ROOST_AGENT_HOOK` from the environment of the agent it
  spawns, so that agent's own installed Roost hooks stay inert inside
  craze and cannot claim the tab out from under it — which is also why a
  craze tab shows one agent rather than two.
- Nothing is reported until craze's session is ready, so a restored
  session never raises a spurious "Turn complete".

See [Reporting directly, without a hook](#reporting-directly-without-a-hook)
below for the rules craze itself follows, and for the wire shape any
other agent driving `tab.agent_report` by hand needs.

## Reporting directly, without a hook

A TUI or script that *is* the process driving the coding agent — craze
is the first, over ACP — has no separate hook to install: it calls
[`tab.agent_report`](../reference/ipc.md#tabagent_report) itself, on
every state change, the same op every installed hook ends up calling
too. The rules below are what any such caller has to get right, stated
once here rather than per integration. `roostctl tab report`
([`cli.md`](../reference/cli.md#tab-report)) is the same op as a verb,
for a script or an agent with no adapter of its own that would rather
shell out than hand-build the JSON.

- **Gate on `ROOST_SOCKET` plus a positive `ROOST_TAB_ID`, exactly as
  every installed hook does.** Both are set only inside a Roost tab; a
  process started outside one — or with its environment stripped by a
  sanitizing launcher — has nothing to report to and no tab to claim.
  Reading `ROOST_TAB_ID` with the variable gone (but the value cached
  from a parent process) risks claiming, or reporting on, some other
  Roost's tab entirely.
- **Claim per session, preserve for state, release on every exit —
  including `SIGHUP`.** `ownership_action` is required on every report
  because "take the tab" and "I already own it" have opposite failure
  modes: `claim` when a session starts, `preserve` for every state
  update in between, `release` when it ends. Roost hangs up the tab's
  tty when the tab closes, so a reporter that forked its agent into its
  own process group has to catch `SIGHUP` itself and release from the
  handler — the tty going away is not something the child necessarily
  sees on its own stdin/stdout ([craze#22](https://github.com/charliek/craze/issues/22)
  tracks this for craze).
- **Never add a field the wire doesn't already name.** `TabAgentReportParams`
  is `deny_unknown_fields`: a report carrying one field the server
  doesn't recognize is rejected *whole* — not the field, the report —
  which reads to the caller as nothing happening at all. Anything that
  doesn't fit the existing axes belongs in `metadata` instead.
- **`metadata` keys are namespaced.** A key a product defines itself is
  prefixed `<product>.` (`craze.provider`, `gx.remote`); a key Roost
  itself defines stays bare snake_case (`model`, `version`). See
  [gx](#gx) below for the fuller statement of this rule.
- **Pin to the oldest server you support, and validate against the
  schema.** `identify.ops` says which *ops* a server serves, not which
  *fields* of an op's params it accepts — a server can list
  `tab.agent_report` and still reject a report carrying a field it
  predates, the same `deny_unknown_fields` whole-report rejection as
  above. There is no field-level capability channel to detect this at
  runtime, so the answer is to know, ahead of time, the oldest server
  version the integration supports and stay inside what that version's
  schema accepts. `roostctl schema` ([`cli.md`](../reference/cli.md#schema))
  prints the pinned [JSON Schema
  bundle](../reference/ipc.md#machine-readable-schema) with no socket
  and no running Roost, so this is checkable offline, against the exact
  server version being targeted.
- **`lifecycle_if` guards a reported lifecycle on the tab's current
  one** — see [`tab.agent_report`](../reference/ipc.md#tabagent_report)
  for the full semantics. It is absent from a server built before the
  release that follows v0.0.19: sending it to one of those rejects the
  whole report, `deny_unknown_fields` again, so an integration that
  needs to support that generation has to guard the transition on its
  own side instead, the way craze does today.

**craze as a worked example.** craze keys ownership on the ACP session
id, claims with `inactive` on session start and metadata `model`,
`craze.provider` and `version`, reports `working`/`waiting`/`finished`/`failed`
as the turn proceeds, and releases on exit — see [craze#25](https://github.com/charliek/craze/issues/25)
for its own tracking issue and test plan for this integration. One
minimal wire example, a claim, a mid-turn preserve, and a release, each
validating against the pinned schema:

```json
{"tab_id": "7", "source": "craze", "session_id": "acp-9f2",
 "ownership_action": "claim", "lifecycle": "working",
 "metadata": {"craze.provider": "cursor-agent", "model": "claude-opus-4"}}
```

```json
{"tab_id": "7", "source": "craze", "session_id": "acp-9f2",
 "ownership_action": "preserve", "lifecycle": "waiting",
 "attention": "set", "title": "Needs input",
 "body": "approve the shell command?"}
```

```json
{"tab_id": "7", "source": "craze", "session_id": "acp-9f2",
 "ownership_action": "release"}
```

## Roost asks once

Roost never writes into an agent's config file before you've said yes.
`agent-hooks` in `config.conf` has three states — a list of agent names,
`off`, or **absent**, which means nobody has answered yet:

- **Absent (a fresh install).** The first time the UI starts with no
  answer on file *and* at least one of the five agents installed, a
  consent dialog opens — the iced **Agent Hooks…** card, the Mac
  **Agent Hooks…** sheet — naming which agents it found and letting you
  pick which to wire, or none. Nothing is written before you answer it.
  It reopens on demand from the command palette (or, on Mac, the View
  menu) as the same **Agent Hooks…** row, bound to the default-unbound
  `agent_hooks` keybind action, so you can revisit the choice later.
- **A list.** At startup — the iced UI, the Swift app, and (on connect)
  a host session — Roost runs the equivalent of `roostctl agent ensure`:
  for every agent the list names whose config directory exists on that
  machine, it makes sure Roost's hook entry is present and current,
  merging in beside whatever else is already in that agent's config.
- **`off`.** Nothing is wired at startup. See [How to opt
  out](#how-to-opt-out) for what this does and doesn't remove.

The iced UI also shows a one-line toast the first time any agent is
*newly* wired on a machine, naming which ones and how to undo it; after
that, refreshes on upgrade are silent. **The Swift `Roost.app` has no equivalent
toast** — its chrome has no transient status surface — so a machine
wired only through the Swift app or `roostctl` stays unannounced until
either the iced UI runs there too, or you check by hand.
`roostctl agent status` and `roostctl doctor`'s `Agents` section are the
durable way to see what's wired without waiting for a toast.

If the startup wiring can't set up an agent that the list names and
that is installed, the iced UI says so in the same toast, for example
`Agent hooks: couldn't set up claude — run roostctl agent status`.
This happens when the agent's config file doesn't parse, isn't the shape
the agent documents, or is a file Roost owns by name but didn't write,
or when the file can't be read or written at all. The toast
comes back on every launch while the problem lasts. To silence it, fix
the file (`roostctl agent status` names it), or drop the agent from the
`agent-hooks` key. An agent Roost couldn't wire is never announced as
wired, so its one-time announcement still comes once the file is fixed.

Every entry Roost installs invokes `roostctl` (or `roost-session` on a
host) indirectly, through the `$ROOST_AGENT_HOOK` environment variable
every Roost tab is given — never a baked-in absolute path — so the exact
same command string works after a relocated install, on a second
machine, or over a host session; see [How
`ROOST_AGENT_HOOK` works](../reference/cli.md#environment).

## How to opt out

One `config.conf` key, read by both UIs and by `roostctl agent ensure`:

```conf
agent-hooks = claude, codex   # a list, off, or absent (nobody has answered yet)
```

The value is a comma list of any of `claude`, `codex`, `grok`, `cursor`,
`opencode` — normalised trimmed, lowercased, de-duplicated, and
reordered into that same canonical order — or the literal `off`. A name
the parser doesn't recognize is dropped with a warning rather than
failing the whole value; the retired `auto`/`on`/`true`/`yes` spellings,
and anything mixing one of those reserved words with a real name, now
parse back as **absent** (unanswered) with a warning, not as "wire
everything". See [`config.md`](../reference/config.md#agent-hooks) for
the full parsing rules.

**`off` means two different things, deliberately.** On the machine
whose `config.conf` says `off`, the UI does nothing at all — it never
opens an agent's config file, at startup or ever. That is the whole of
what the key does locally, and it is worth being exact about the part
that surprises people: **restarting the UI removes nothing.** Entries
already on disk stay exactly where they are until you run one of the two
commands that take them out, or reopen the consent dialog and choose
`off`/fewer agents there:

```bash
roostctl agent uninstall --all   # or a single agent name
roostctl agent ensure            # reads the same key; on `off`, unwires
```

`agent ensure` is the same verb the UIs run at startup with `--startup`
(never removing); run bare, by hand, while the key says `off` (or names
fewer agents than are wired), it removes Roost's entries from whatever
the key does not allow. Nothing else does — a launch never undoes a hook
someone added by hand, and never reacts to a key that changed while the
app was closed. See [Remote hosts](#remote-hosts) for how a host session
handles the same key differently — it can only ever be **raised**, never
lowered, by a connecting client.

## How to override

**Wire one agent regardless of the config key** — explicit always wins
over `agent-hooks = off`:

```bash
roostctl agent install codex
```

**Hand-edit beside Roost's entries.** Because ownership is exact string
match (see [Ownership](#ownership) below), anything you add next to
Roost's hook group — another handler on the same event, a `matcher`, a
comment in a TOML file — is untouched by `agent ensure`, `agent
install`, or `agent uninstall`. Editing Roost's *own* entry (say,
changing its timeout) makes it stop being recognized as Roost's; the
next `ensure` treats the agent as needing a refresh, and doctor calls it
out as a modified entry rather than silently overwriting your edit or
silently leaving it be.

## Install

`roostctl agent ensure --startup` is exactly what the UIs run at
startup — wire and refresh what the key names, remove nothing. The
other verbs are the manual controls doctor points at, including the one
that answers the consent question from a terminal:

```bash
roostctl agent status                       # per agent: present, wired@vN, up to date
roostctl agent ensure [--json] [--startup]  # reconcile to `agent-hooks`; --startup never removes
roostctl agent set claude,codex             # via the running UI: set the key here, raise every connected host
roostctl agent set claude,codex --local     # set `agent-hooks` here directly, with nothing running
roostctl agent install <agent>|--all
roostctl agent uninstall <agent>|--all
```

Every verb but bare `set` reads and writes dotfiles directly, so it
works with nothing running, which is exactly when a new machine needs
it; `agent status` reports each agent's file-level wiring — not the
state record, which only supplies the integration version — so deleting
`<config dir>/roost/agent-hooks.json` by hand doesn't make a correctly
wired agent lie about itself. See [`cli.md`](../reference/cli.md#agent-subcommands)
for `set`'s full behavior, including what it does over a running UI.

`roostctl claude install` remains a bare alias of `agent install claude`
(exit 0 when already wired). It no longer writes
`~/.config/roost/claude-settings.json` or prints a shell alias — see
[Legacy Claude settings](#legacy-claude-settings) if you're migrating off
that older mechanism.

Every write goes through one advisory lock per agent config directory,
so the iced UI, the Swift app, `roostctl`, and a remote connect can all
run `ensure` at the same time without racing each other; a plan
re-checks the file's content immediately before writing and skips
(reporting `changed-underneath`) rather than clobbering a file that
moved in between — Claude rewrites its own `settings.json` on its own
schedule, so this is a real race, not a theoretical one.

## Ownership

A tab is "owned" by whichever agent's hook last reported activity on
it, via `tab.agent_report`'s `(source, session_id)` pair. Doctor's
`owning` checks read this off the running UI's tab list — there is no
durable "ever observed" store, so they can only say who owns a tab
*right now*, not whether an agent has ever fired here.

Separately — and this is the sense of "ownership" the install engine
itself cares about — **an entry in an agent's config file is Roost's
if, and only if, its command string is byte-for-byte one Roost has ever
produced**, at any past integration version. That is deliberately not a
substring test:

- A hook you wrote yourself that happens to mention `$ROOST_AGENT_HOOK`
  is yours. It is never touched, and it survives every `ensure` and
  every `uninstall`.
- A Roost entry you've since hand-edited (a different timeout, an added
  `matcher`) stops being recognized as Roost's the moment it no longer
  matches exactly. It is left exactly where it is — never rewritten,
  never removed — and doctor names it as a modified entry rather than
  pretending it's current.

Only a file Roost itself created is ever deleted on uninstall, and only
the state record can say which those are; a `{}` or an empty file that
predates Roost is written back empty rather than removed.

## When a tab stays running

An agent that is hard-killed — `kill -9`, a crash, a closed laptop lid
over SSH — fires no `Stop`/`SessionEnd` hook, so the tab keeps whatever
lifecycle it last reported (`running`, `needs_input`, …) with nothing to
tell Roost otherwise.

**What clears it.** Ownership is deliberately not TTL'd — Claude fires
no periodic hook, so a long tool call would look stale and get released
mid-turn — so the only thing that clears a stuck lifecycle is the
shell's own OSC 133 prompt marks. Reaching a fresh prompt (`A`/`B`) or a
command ending (`D`) drops the lifecycle to `inactive` while *keeping*
ownership as a label, so the tab falls through to shell-derived state
instead of staying stuck. The marks clear the lifecycle because the
shell has regained control of the terminal, not because the agent is
proven gone — a suspended agent (`Ctrl-Z`) also returns the shell to a
prompt while it's still alive in the background.

**Which shells emit those marks.** Roost's bundled integration
(`crates/roost-engine/resources/shell-integration/`) wires them for zsh
and bash: zsh's
`preexec`/`precmd` hooks fire the `C` and `D` marks unconditionally;
bash's `PROMPT_COMMAND` fires `D` on every version, but the `C`
(command-start) mark needs bash ≥ 4.4 — `PS0` is silently ignored on
older bash, including macOS's stock `/bin/bash` 3.2. A shell with no
prompt marks at all — dash, plain `sh`, or bash/zsh with the
integration disabled — never clears a stuck lifecycle on its own.

**Manual clear.** From any shell:

```bash
roostctl tab set-state --tab <id> --state none
```

This claims ownership as `manual` and releases it in the same step,
falling the tab through to shell-derived state (see
[`cli.md`](../reference/cli.md#tab-set-state)).

**fish.** Tested with fish 4.9.3: fish marks its own prompts and
commands (OSC 133) with no Roost integration installed, so a fish tab's
shell state reads `at_prompt` from the first prompt, and a tab left
`running` by an agent that was killed clears as soon as fish draws its
next prompt — the same as Roost's own bash integration. Older fish
releases were not tested.

There is no shell-agnostic failsafe today — nothing in `crates/` reads
the PTY's foreground process group. A `tcgetpgrp`-based one, which would
cover dash/`sh` too, is tracked as future work in
[#519](https://github.com/charliek/roost/issues/519).

## Codex trust

Codex additionally records a `trusted_hash` per hook handler under
`[hooks.state]` in `config.toml` — the mechanism that stops it asking
"Hooks need review — trust all?" on the next launch. Writing that hash
is Roost approving its own hook command on your behalf, ahead of the
review codex's own dialog exists to gate; see [Security](#security)
below for that trade-off stated plainly.

The trust keys are **index-based** and built from the absolute
`hooks.json` path, so two things move them even though nothing about
Roost's own hooks changed:

- **Reordering** — a hook group you (or another tool) insert ahead of
  Roost's shifts Roost's index, so the stale key no longer matches and
  codex shows one review dialog on the next launch.
- **A moved `CODEX_HOME`** — the path is part of what's hashed, so
  relocating it (or a dotfile sync that changes it per machine, without
  `allow_symlinked_codex_home = true`) has the same effect.

Both cost exactly one dialog; the next `roostctl agent ensure` or `agent
install codex` recomputes and rewrites the stale keys, and doesn't
require anything from you beyond running it. Doctor's
`agent.codex.trust` check compares the hash codex would compute for the
handler Roost actually installed against what's on disk right now, so a
hand edit, a moved home, or a codex-side change to the hash formula
itself is diagnosed by name instead of reappearing as an unexplained
dialog with no cause attached.

## Legacy Claude settings

Before this feature existed, `roostctl claude install` wrote a
Roost-owned file at `~/.config/roost/claude-settings.json` and asked you
to alias `claude` to pass `--settings` at it. That file is retired:
`claude install` is now a bare alias of `roostctl agent install claude`,
which merges hook entries directly into Claude's own
`~/.claude/settings.json` instead.

If both the old file and the alias are still active, every Claude hook
event is delivered twice — harmless to Roost's state, but wasteful, and
the old file's absolute path is wrong on any other machine. Remove it:

1. Delete `~/.config/roost/claude-settings.json` (or run `roostctl
   agent uninstall claude`, which removes it when it still matches the
   shape `claude install` used to write, and leaves it alone — with a
   warning — if you've since hand-edited it).
2. Remove the `alias claude=…` line it asked you to add from your shell
   rc (`.bashrc`, `.zshrc`, `.bash_profile`, or fish's `config.fish` /
   `alias --save` output).

`roostctl doctor`'s `agent.claude.legacy_settings` check watches for
both independently and says which one (or both) it found.

## The guarantee

Stated as plainly as the mechanism allows, because "Roost edits your
agent's config file automatically" is the most consequential thing this
feature does:

**Off and uninstall remove only what Roost wrote.** Whether that
removal can restore the file byte-for-byte depends on the format:

- **TOML** (codex's `config.toml`) is edited with `toml_edit`, so it is
  **byte-preserving outside the tables Roost touches** — comments and
  layout elsewhere in the file survive untouched.
- **JSON** (every other agent's file) cannot be: parsing and
  re-serializing loses the original bytes, full stop. The guarantee
  there is **semantic**, not byte-exact: every value Roost did not add
  is equal after the write, key order is preserved, numbers keep the
  token the file originally spelled them with, and the file's indent
  unit, line-ending, and trailing-newline conventions are detected and
  reused. **A file is written only when the parsed value would actually
  change** — an already-current file isn't touched at all.

  What that does *not* cover: escape spellings (`c` comes back as
  `c`, `a\/b` as `a/b`) and any layout the printer doesn't reproduce — a
  compact file, an inline array, a blank line between keys — are
  normalized the first time Roost has to write. A file already in the
  printer's own layout round-trips byte for byte; one that wasn't comes
  back semantically equal, reformatted, and an uninstall can't undo that
  reformatting.

Two more properties worth knowing: bytes that aren't valid UTF-8 are a
**skip**, never a lossy substitution — a lossy decode would risk
silently destroying something like an API token on the next write — and
every write is atomic (temp file + rename, in the same directory,
through a symlink to its real target rather than over it), so a crash
mid-write never leaves a torn config file.

## Inert outside Roost

The installed command is unconditional — it's the same string whether
or not Roost is running:

```sh
sh -c 'if [ -n "${ROOST_AGENT_HOOK:-}" ] && out=$("${ROOST_AGENT_HOOK:-}" agent-hook <agent> 2>/dev/null); then [ -n "${out:-}" ] || out="{}"; printf "%s" "${out:-}"; else cat >/dev/null; printf "{}"; fi'
```

Every variable carries a `:-` default deliberately, the local `out`
included. grok doesn't hand the command to a shell unexamined — it
checks every `$` reference against its environment first and refuses
to run the hook at all when one is unset, drawing a red `hook not
executed: required env var(s) not set` row on every tool call in any
terminal that isn't Roost. The `${NAME:-}` form passes that check and
reaches the shell unchanged, so grok runs the command and the fallback
below answers quietly. It's identical POSIX shell for `sh`, `dash` and
`bash`, so no other agent notices. Not every agent shells out, so this
guarantee is only as good as the last time each agent was actually run
in a plain terminal — which is why that run is part of the release
checklist, per agent.

Outside a Roost tab, `ROOST_AGENT_HOOK` is unset: the command drains
stdin, prints `{}`, and exits 0 — inert, and safe for the agent (a
decision hook like Claude's `PermissionRequest` needs exactly this
shape to avoid blocking the dialog), but **not free**. Every hook event,
on every machine the entry is installed on, spawns a `sh` and a `cat`
regardless of whether Roost is running there at all. That's the price
of a host-independent, dotfile-syncable entry with no absolute path
baked in — worth naming plainly rather than leaving as a surprise the
first time someone notices the extra process per tool call.

## Remote hosts

**Every machine, including a host, has exactly one `agent-hooks`
setting: the key in its own `config.conf`.** There is no separate host
stance. What plan 064 changed is what a *connecting client* may do to
that key — and the answer, stated plainly because it's the contentious
part, is: **raise it, never lower it.**

Right after connecting, a client whose own `agent-hooks` names at least
one agent sends that list to the host as the `session.set_agent_hooks`
op ([`ipc.md`](../reference/ipc.md#sessionset_agent_hooks)). The host
**unions** the list into whatever its own key already says, then wires
whatever the union now allows:

- **A client whose own key is `off`, or still unanswered, sends nothing
  at all.** It has no allow-list to widen a host with, so it doesn't try
  — and it also cannot narrow or unwire the host, on this connect or
  ever, through this op.
- **A host whose key is explicitly `off`, or unanswered, is raised
  anyway.** This is the one genuinely surprising part: a host has no
  screen to put a consent dialog on, so the client in front of the user
  — reaching the host over the same-UID socket boundary every other
  op already trusts — is the only authority there is. Connecting with
  `agent-hooks = claude` to a host whose key says `off` wires Claude
  there, full stop.
- **Lowering a host only ever happens on that machine.** `roostctl
  agent ensure`/`uninstall` run there (over SSH, say), or its own
  consent dialog if one is ever opened on it, is what takes the key —
  and the files — back down. Once lowered, it holds until a more
  permissive client connects and raises it again.
- **Two clients raising different lists both win, additively.** Neither
  narrows what the other asked for — the union just grows. The host's
  state record stores which client (`by`) raised which agent and when,
  so `roostctl agent status` run on the host names who asked for what.
  This reverses plan 046's original rule, where a disagreeing
  `agent-hooks` was last-writer-wins and an `off` client actively
  unwired the host on every connect — that behavior is gone.
- **Agents already running when you first connect pick up the hooks on
  their next launch**, not retroactively — a `claude` process started
  before the host was raised is still reading whatever hooks were on
  disk when it started.

No new remote command surface is added by this: any same-UID client
that can reach a host's socket can already run arbitrary commands there
via `tab.open` — that op has always been lease-free, and as of plan 057
(R15) `tab.write` is too, so wiring a hook command is not a new
capability — it's the same one, applied to a dotfile instead of a
shell.

## Security

Three things worth stating as a stance rather than leaving implicit:

- **The `$ROOST_AGENT_HOOK` indirection is not a security boundary.** A
  process running inside a Roost tab could read that variable and
  invoke the binary it names directly — but it could just as easily run
  that same binary without reading the variable at all, since it's
  already running with the same privileges the hook process would have.
  The indirection exists so the installed command is host-independent
  and dotfile-syncable, not to fence anything off.
- **Writing codex's `trusted_hash` bypasses a human gate on purpose.**
  Codex's review dialog exists so a person confirms a hook change
  before it runs with their authority; pre-computing the hash Roost's
  own entry would produce and writing it in ahead of time is Roost
  approving its own command for you. That's a deliberate trade-off in
  favor of "every agent works with zero manual steps," not an oversight
  — and it only ever covers the exact command Roost installed; anything
  else still gets codex's normal review. `agent.codex.trust` is the
  check that keeps this honest by flagging drift instead of silently
  re-trusting on every run.
- **A bare `opencode` in a Roost tab exposes that session's server on
  loopback.** Roost's opencode plugin fronts the TUI's in-process server
  with a `127.0.0.1` listener so a client can actually drive the session
  Roost reports (see [OpenCode keys](#opencode-keys)) — and that server
  is the whole surface: the global session store, tool execution,
  permission replies. Any process on the same host can reach it,
  unauthenticated unless `OPENCODE_SERVER_PASSWORD` is set to a **non-empty**
  value — an empty one means "no auth" to opencode itself, and so to this
  front end. Roost's own
  sockets are unchanged and nothing binds off loopback — but this is a
  real widening of what a tab exposes, which is why
  `ROOST_OPENCODE_NO_SERVER=1` turns it off.

The first two points introduce no capability a same-UID client didn't
already have: on a host, any client that can reach the socket can
already run arbitrary commands there via `tab.open`. The third does
widen the surface, on purpose, and the opt-out is the answer to it.

## gx

gx is a fork of grok that shares grok's config file and reports through
the same adapter — it shows up as `source: "grok"`, same as stock grok;
[Per-agent caveats](#per-agent-caveats) below covers where gx's behavior
diverges from grok's.

gx additionally announces a **remote lane** — an HTTP control surface
for the running session — by stamping `gxRemote` (its own loopback base
URL, e.g. `http://127.0.0.1:2421`) onto every hook payload once that
lane comes up. Roost forwards it, when it parses as a **token-free
loopback base URL** — `http://127.0.0.1:<port>`, `localhost`, or
`[::1]`, and nothing after the port (no path, query, or fragment) —
as the `gx.remote` key in the tab's `metadata`; anything else (a query
string, a non-loopback host, `https`) is dropped rather than stamped.
`roostctl doctor` prints the value when present.

`gx.remote` is a **naming convention, not a new mechanism**: `metadata`
is already the open extension channel every adapter writes into
([`ipc.md`](../reference/ipc.md#tabagent_report)), and this is the rule
for using it without collisions — a key owned by one product is
prefixed `<product>.` (`gx.remote`), while a key roost itself defines
stays bare snake_case (`model`, `session_title`).

Treat the key as a **discovery hint, not liveness**. gx's remote-lane
address is a process-global set once and never cleared, so it never
tells you the lane died; and `metadata` itself is merge-only with no
delete channel, so `gx.remote` can outlive the lane that announced it,
and a same-process restart on another port is never reflected. The
only thing that removes it is the reset path: a new `SessionStart`
replaces `metadata` wholesale, so a stock-grok session started on the
same tab afterward drops the key. It is absent to begin with on stock
grok and on a gx run with `--no-leader` or `GX_REMOTE_DISABLE=1`.

A consumer must still follow gx's own client contract before treating
the lane as usable — the key alone is never enough to act on: call the
lane's token-free `GET /v1/healthz`, compare its `instanceId` against
`$GROK_HOME/gx-remote.json`, and only then use the bearer token from
`$GROK_HOME/gx-remote.token`. Roost carries neither the instance id nor
the token; `gx.remote` is just the URL.

The agents palette does not render `gx.remote` — a bare loopback URL has
no in-terminal action to attach it to; revisit if a roost client action
ever uses the lane. gx does not get its own `source`/`Agent` variant
today; that's warranted only if its hook vocabulary diverges from
grok's in a way the shared adapter can't map correctly, or it stops
sharing `$GROK_HOME` with grok.

## OpenCode keys

OpenCode's adapter writes four keys into the owning tab's `metadata`.
All four are bare snake_case under the convention [above](#gx): they are
Roost's own, read off opencode's event bus by the plugin Roost ships,
not a product stamping its name on a shared channel.

| Key | Source |
|---|---|
| `model` | `session.created` → `info.model.id` |
| `agent` | `session.created` → `info.agent` |
| `version` | `session.created` → `info.version`, the opencode build |
| `server_url` | the loopback address the session can be driven at |

### `server_url`

A bare `opencode` binds no socket at all — the TUI talks to its server
in-process, and opencode's `server.port` setting only applies to
`opencode serve` — so there is nothing for a client to dial. Roost's
plugin runs *inside* that server process and is handed a client whose
`fetch` is the in-process app, so it puts a loopback listener
(`127.0.0.1`, port 0) in front of that fetch and reports the address as
`server_url`. What a client reaches there is opencode's own REST and SSE
surface.

- **In-process vs external.** When opencode is *already* listening on a
  socket of its own — `opencode serve`, `opencode web`, `--port`, and
  the rest — the plugin serves nothing and reports opencode's own
  address instead. It tells the two apart from the client opencode hands
  the plugin, which carries an in-process `fetch` only when there is no
  real listener, rather than by reading the command line.
- **Loopback only.** Roost records `server_url` only when it parses as
  a token-free loopback base URL: `http://127.0.0.1:<port>`,
  `localhost`, or `[::1]`, with nothing after the port. An external
  server started with `--hostname 0.0.0.0` announces an address that
  fails that check and is dropped rather than stamped — that session is
  status-only.
- **The password rule.** When `OPENCODE_SERVER_PASSWORD` is set to a non-empty value, the
  loopback front end demands the same Basic credentials opencode's own
  server does (username from `OPENCODE_SERVER_USERNAME`, default
  `opencode`) and answers `401` otherwise. Roost carries no credentials
  — `server_url` is just the URL — so a consumer without them reads a
  password-protected session as status-only.
- **The opt-out.** `ROOST_OPENCODE_NO_SERVER=1` in the tab's
  environment: no listener, no key, status reporting unaffected. The
  same outcome if the runtime has no `Bun`, or if the listener fails to
  start — one line on opencode's log, and nothing else changes.
- **What the opt-out does *not* cover.** It suppresses the loopback
  listener **Roost creates**. It does not suppress reporting a server
  *you* started: the external case is checked first, so an opencode
  already listening still reports its own address as `server_url` even
  with the opt-out set. That ordering is deliberate — Roost stands up no
  server there, which is the whole of the variable's promise, and the
  address it reports is a listener you explicitly asked for. If you want
  no `server_url` at all, don't start one.
- **The mid-session limitation, and its bound.** `server_url` rides the
  ownership claim on `session.created` *and* the key-by-key upsert on
  every event after it — so a plugin that loads mid-session onto a tab
  the same opencode session already owns lands the key on the very next
  forwarded event, whatever kind it is. The limitation is only the
  **unowned** tab: an upsert is authorized against the current owner, so
  against a tab nobody owns it creates nothing, and `opencode attach` or
  an install that happened while a session was already running never
  sees the `session.created` that would claim one. There the key first
  lands at the **next** `session.created`, and until then that session
  is status-only.

Treat it the way [`gx.remote`](#gx) is treated: a discovery hint, not
liveness. `metadata` is merge-only with no delete channel, so the key
outlives the listener that announced it — a restarted `opencode` in the
same tab publishes a new port on its next `session.created`, and until
then the old one is stale. Dial it before believing it.

## Per-agent caveats

- **cursor has no blocked state.** `beforeShellExecution` fires roughly
  0.1 s before `afterShellExecution` when a command is auto-approved, so
  there is no reliable way to distinguish "waiting on your approval"
  from "running" — an accepted gap, not a bug to file. Every cursor
  `stop` status (including the two an Esc produces in a row) maps to
  `finished`, with the raw status carried in `detail`, because reading
  `status: error` as a failure would misreport every interrupt as one.
- **`opencode attach` reports against the server's tab.** OpenCode's
  plugin runs inside the `opencode` server process, which inherited
  whichever tab's `$ROOST_TAB_ID` started that server — so attaching to
  a server another tab already started reports activity there, not on
  the tab you attached from. There's no reliable fix from the plugin
  side (a session's `directory` matching your tab's cwd isn't a safe
  enough signal), so this is documented rather than papered over.
- **gx leader mode misattributes hooks to the first tab, and a quit gx
  keeps its tab owned** ([grok-build#14](https://github.com/charliek/grok-build/issues/14)).
  gx's hooks run inside the `gx agent leader` process, not the TUI
  itself, and that process inherits `ROOST_TAB_ID` from whichever TUI
  spawned it — so every further gx TUI that attaches to the same leader
  reports its events against the *first* tab (its `SessionStart` claims
  that tab), and the second TUI's own tab stays `inactive`, the same
  shape as the `opencode attach` caveat above. Separately, `SessionEnd`
  never fires for a leader-resident session, so quitting a leader-backed
  gx leaves its tab owned at whatever lifecycle it last reported,
  instead of releasing to `inactive`. Stock grok (leader mode off) is
  unaffected by either.
- **grok's Stop gate leaves a residual "finished" window on the *first*
  blocked fire.** A blocking Stop hook that blocks the very first `Stop`
  of a turn shows the tab as `finished` (with a "Turn complete" banner)
  from that fire until the next tool event or the next `Stop` — which,
  once the gate has fired at least once, carries `stopHookActive: true`
  and correctly reads as `working` again. In that gate case the user
  never sees a second "Turn complete"; instead they see gx's own
  `idle_prompt` banner roughly a minute after the turn actually ended.
  If the 8-continuation cap is hit, gx forces a stop with no hook fired
  at all, so the tab stays `working` until `idle_prompt` settles it. A
  passive observer — which is all roost's adapter is — cannot close this
  gap without delaying every turn's "Turn complete" by that same minute,
  gated or not; that trade was rejected.
- **grok's `agent_error` Notification is deliberately silent.** It fires
  immediately before the `StopFailure` that reports the same failure,
  so banner-ing both would show the same error twice; only the
  `StopFailure` banner (with gx's `errorDetails` text) reaches the tab.
- **grok subagents never claim the tab.** A subagent (from
  `spawn_subagent`) runs as its own session id, stamps `subagentType` on
  every event, and never fires its own `SessionStart` — so its reports
  never match the tab's owning `(source, session_id)` pair and the
  server drops them on the ordinary `preserve` mismatch path. No adapter
  filter is needed.
- **codex re-trusts on reordering or a moved path.** See [Codex
  trust](#codex-trust) above — either one costs a single review dialog,
  cleared by the next `ensure`.
- **codex `/rename` doesn't reach the tab label on codex 0.153 and
  earlier.** Roost's tab label mirrors whatever the program writes into
  the terminal title, and codex's default title items on those versions
  are only `activity` and `project` — the thread name isn't in it, so a
  rename changes nothing. Add `terminal_title = ["activity",
  "thread-title", "project-name"]` under `[tui]` in `config.toml` and the
  rename lands in the label at once (an unnamed thread then shows its
  UUID until you name it). Newer codex adds a `thread-name` item that's
  omitted when unnamed and puts it in the default set, so no config is
  needed there. Claude, grok and cursor already put the session name in
  the title.
- **gx shares grok's file.** grok and its fork gx read the same
  `$GROK_HOME`, so one install (`roostctl agent install grok`) wires
  both binaries at once, and uninstalling `grok` removes the file both
  share. See [gx](#gx) above for gx-specific behavior.
- **cursor (and optionally grok) also execute Claude's hook format.** cursor always
  loads `~/.claude/settings.json` (`claudeUserHooks`), and grok can be
  configured to; with Roost's Claude entries installed, both would
  otherwise run `agent-hook claude` on their own events and report
  under `source: claude` with the wrong session id. The Claude adapter
  defends against this by rejecting any payload carrying cursor's
  `conversation_id`/`cursor_version` fields or grok's camelCase
  `hookEventName` twin, verified by a cross-adapter test that replays
  every agent's fixtures through every other agent's adapter and
  asserts zero claims.

## Troubleshooting

Run `roostctl doctor -v` first — its `Agents` section covers most of
what you'd otherwise have to guess at, per agent:

- `agent.hook_binary` — is `$ROOST_AGENT_HOOK` set and does it resolve
  to something executable, from inside a tab.
- `agent.<name>.wired` — is Roost's entry present, and at which
  integration version (a stale version means the next `ensure` still
  has something to do, not that anything is broken).
- `agent.<name>.owning` — does that source currently own a tab on this
  UI (not a durable "ever observed" record — just right now).
- `agent.codex.trust` — does the trust hash on disk match what codex
  would compute for the handler Roost installed.
- `agent.claude.legacy_settings` — is the retired
  `~/.config/roost/claude-settings.json` file or its shell alias still
  around.

`-v` breaks the section out per agent; the plain summary line names
whichever fact is most actionable. Every `warn`/`fail` links back to the
exact section above that explains it.

If a hook genuinely never fires, check first whether something strips
the tab's environment before the agent sees it — a wrapper using `env
-i`, a `sudo` re-exec, or a sanitizing launcher all make the installed
hook silently inert. Roost has no durable "ever observed" store, so
doctor can only say whether this UI has seen the agent fire *this
session* — a wired-but-quiet agent that has genuinely never run in a
Roost tab yet looks the same as one whose hook can't reach Roost at
all.
