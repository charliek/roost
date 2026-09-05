# Agent Hooks

Roost drives the tab dot, the sidebar rollup, and the desktop banner for
five coding agents — Claude Code, Codex, OpenCode, grok (and its fork
gx), and cursor-agent — by wiring one hook entry per lifecycle event into
each agent's own configuration file. Wiring happens automatically the
first time the Roost UI starts (`agent-hooks = auto` in `config.conf`,
the default); nothing here is a per-machine manual step, and it works
the same way on a remote host session.

## Supported agents

Every agent reports through the same four-axis model — ownership,
lifecycle, attention, and free-form metadata — over one wire op
(`tab.agent_report`, [`ipc.md`](../reference/ipc.md#tabagent_report)).
What differs is where each signal comes from:

| Agent | `source` | Config file Roost writes into | Blocked signal | Turn-end signal | Interrupt signal |
|---|---|---|---|---|---|
| Claude Code | `claude` | `~/.claude/settings.json` (or `$CLAUDE_CONFIG_DIR`) — merged in beside your own hooks | `PermissionRequest` (immediate) | `Stop` | none — the post-turn `idle_prompt` notification is the only later signal, guarded so it can't overwrite a real `waiting`/`failed` |
| Codex | `codex` | `~/.codex/hooks.json` + `[hooks.state]` in `config.toml` (or `$CODEX_HOME`) | `PermissionRequest` | `Stop` | `Interrupt` |
| grok / gx | `grok` | `$GROK_HOME/hooks/roost.json` (default `~/.grok`) — a file Roost owns outright | `Notification` `notificationType: permission_prompt` | `Stop` | `StopCancelled` |
| cursor-agent | `cursor` | `~/.cursor/hooks.json` (or `$CURSOR_CONFIG_DIR`) — merged in beside your own hooks | none — an accepted gap, see [Per-agent caveats](#per-agent-caveats) | `stop` | `stop` (same event as turn-end; see caveats) |
| OpenCode | `opencode` | `~/.config/opencode/plugins/roost-agent-state.js` (or `$OPENCODE_CONFIG_DIR`) — a plugin Roost owns outright, not a command hook | `permission.asked` / `question.asked` | `session.idle` | `session.error` |

Claude and cursor also load hooks from `~/.claude/settings.json`
themselves (grok can be configured to, and cursor always does via its
`claudeUserHooks`), so both adapters positively reject any payload that
carries the other's telltale fields rather than relying on which file it
came from — see [Per-agent caveats](#per-agent-caveats).

## What Roost does automatically

At startup — the iced UI, the Swift app, and (on connect) a host
session — Roost runs the equivalent of `roostctl agent ensure`: for
every one of the five agents whose config directory exists on that
machine, it makes sure Roost's hook entry is present and current,
merging in beside whatever else is already in that agent's config. The
first time any agent is newly wired on a machine, a one-line toast names
which ones and how to undo it; after that, refreshes on upgrade are
silent. `roostctl agent status` and `roostctl doctor`'s `Agents` section
are the durable way to see what's wired without waiting for a toast.

Every entry Roost installs invokes `roostctl` (or `roost-session` on a
host) indirectly, through the `$ROOST_AGENT_HOOK` environment variable
every Roost tab is given — never a baked-in absolute path — so the exact
same command string works after a relocated install, on a second
machine, or over a host session; see [How
`ROOST_AGENT_HOOK` works](../reference/cli.md#environment).

## How to opt out

Two `config.conf` keys, read by both UIs and by `roostctl agent ensure`:

```conf
agent-hooks = auto        # auto (default) | off
agent-hooks-skip = cursor, codex   # comma list; never wired even under auto
```

`agent-hooks-skip` takes any of `claude`, `codex`, `grok`, `cursor`,
`opencode`; a name it doesn't recognize is reported and otherwise
ignored, never fatal — one typo shouldn't turn into "nothing is wired
and nothing says why." See [`config.md`](../reference/config.md#agent-hooks)
for the full parsing rules (accepted spellings, what an unknown value
falls back to).

**`off` means two different things, deliberately.** On the machine
whose `config.conf` says `off`, the UI does nothing at all — it never
opens an agent's config file, at startup or ever. That is the whole of
what the key does locally, and it is worth being exact about the part
that surprises people: **restarting the UI removes nothing.** Entries
already on disk stay exactly where they are until you run one of the two
commands that take them out:

```bash
roostctl agent uninstall --all   # or a single agent name
roostctl agent ensure            # reads the same key; on `off`, unwires
```

`agent ensure` is the same verb the UIs run under `auto`; run by hand
while the key says `off`, it removes Roost's entries from every agent it
can see. Nothing else does. The split exists so a config switch can mean
"stop wiring from now on" without every process that merely *reads* the
config key being trusted to also *rewrite* dotfiles on its own — see
[Remote hosts](#remote-hosts) for how this plays out over a host session,
where the split reverses.

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

`roostctl agent ensure` is exactly what the UIs run at startup. The
other three verbs are the manual controls the startup toast and doctor
point at:

```bash
roostctl agent status              # per agent: present, wired@vN, up to date
roostctl agent ensure [--json]     # wire everything agent-hooks/-skip allow
roostctl agent install <agent>|--all
roostctl agent uninstall <agent>|--all
```

None of the four dials a running UI — they read and write dotfiles
directly, so they work with nothing running, which is exactly when a
new machine needs them. `agent status` reports each agent's file-level
wiring — not the state record, which only supplies the integration
version — so deleting `<config dir>/roost/agent-hooks.json` by hand
doesn't make a correctly wired agent lie about itself.

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

A host session (`roost-session`) has no `config.conf` of its own — the
connecting client's config is the only authority a session has for what
"wired" should mean there. So the client sends its `agent-hooks` /
`agent-hooks-skip` values to the host as the `session.set_agent_hooks`
op right after every `session.connect`
([`ipc.md`](../reference/ipc.md#sessionset_agent_hooks)):

- **Connecting wires the host.** With `agent-hooks = auto` (the
  default), connecting to a host brings its agent hook files in line
  with your config, the same way local startup does.
- **`off` on a host means unwire, not abstain.** This is the one place
  the local/remote split in [How to opt out](#how-to-opt-out) reverses:
  locally, `off` only stops *future* wiring because there's a human at
  the keyboard who can run `agent uninstall` themselves; on a host,
  nobody is going to SSH in and clean up by hand, so the client sending
  `off` actively removes Roost's entries there. Reconnect with
  `agent-hooks = off` and the host comes clean.
- **Agents already running when you first connect pick up the hooks on
  their next launch**, not retroactively — a `claude` process started
  before Roost wired the host is still reading whatever hooks were on
  disk when it started.
- **Two clients with different configs flip the host's files on every
  reconnect.** This is last-writer-wins, by design, not a bug: the
  host's state record stores which client (`by`) wired or unwired an
  agent and when, and `roostctl agent status` run on the host names the
  most recent flip so the oscillation is at least diagnosable.
  Reconciling disagreeing clients against one host is filed as future
  work, not solved here.

No new remote command surface is added by this: a client that already
holds a host's lease can run arbitrary commands there via `tab.open`, so
wiring a hook command is not a new capability — it's the same one,
applied to a dotfile instead of a shell.

## Security

Two things worth stating as a stance rather than leaving implicit:

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

Neither point introduces a new capability a lease holder didn't already
have: on a host, whoever holds the session's lease can already run
arbitrary commands there via `tab.open`.

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
- **codex re-trusts on reordering or a moved path.** See [Codex
  trust](#codex-trust) above — either one costs a single review dialog,
  cleared by the next `ensure`.
- **gx shares grok's file.** grok and its fork gx read the same
  `$GROK_HOME`, so one install (`roostctl agent install grok`) wires
  both binaries at once; there is no separate `gx` source or adapter,
  and uninstalling `grok` removes the file both share.
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
