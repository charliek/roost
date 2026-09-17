# Drive Roost from scripts and agents

Roost serves one JSON socket that everything else — the UI's own
buttons and hotkeys, `roostctl`, an agent skill — drives. This guide is
for a script or an agent driving that socket, mostly through
`roostctl`. See [`reference/cli.md`](../reference/cli.md) for the full
verb-by-verb reference and [`reference/ipc.md`](../reference/ipc.md)
for the wire format itself, if you're talking to the socket directly.

## The model

A **project** is a named working directory in the sidebar, holding
**tabs**. A tab is one terminal running one shell or command — Roost
has one window, no splits, no panes. Every tab has a `state`: `none`,
`running`, `needs_input`, or `idle`.

A plain shell only reaches `running` and `none`, and only with shell
integration — Roost loads the OSC 133 prompt marks for zsh and bash 4.4
or newer: `running` while a foreground command runs, `none` at its
prompt. A shell without the marks stays `none` the whole time (`tab
list --json`'s `shell_state` reads `unknown`), so wait on a shell's
*output text*, not its state, unless you know shell integration is
loaded. An agent whose hooks report to Roost — see [Agent
Hooks](agents.md) — drives the state itself: `running` during its
turn, `needs_input` when it's waiting on you or its turn failed,
`idle` once its turn ends.

## Ids and environment

Every shell Roost spawns gets `ROOST_TAB_ID` (its own tab) and
`ROOST_SOCKET` (the socket to dial) in its environment:

```bash
printf '%s\n' "${ROOST_TAB_ID:-not in a Roost tab}"
roostctl --socket "$ROOST_SOCKET" set-title --tab "$ROOST_TAB_ID" --title "building…"
```

Tab and project ids are integers, written as JSON strings (`"7"`).
They belong to **this running Roost**: a restart, or a switch of the
local backend between in-process and a session (see [Host
Sessions](host-sessions.md#switching-the-local-backend)), mints new
ids for every tab, so re-resolve an id after either rather than
holding onto one across a process boundary.

## `identify` and `ops`

Before anything else, check that a Roost answers, and see what it can
do:

```bash
roostctl identify --json
```

`identify.ops` lists the operations this socket serves *right now*.
`open` and `project ensure` refuse `unsupported` when `ops` is missing
`project.ensure` — today, the Swift Mac app; `wait` falls back to
polling and `events` fails outright when `ops` lacks
`events.subscribe` and there's no session backing the local tabs. See
[the Mac paragraph](#the-mac-app) below for the concrete shape of
that gap. `identify.instance_id` names the process: it changes on
every restart, and a resumed `wait`/`events` connection checks it to
tell "the same Roost, still running" from "a different one answered".

## The four verbs

### `open`

Find-or-create a project by exact name, then open a tab in it —
`project.ensure` followed by `tab.open`, atomic per call so two
callers racing the same project name converge on one project (#221)
instead of each creating their own:

```bash roost-recipe
out=$(roostctl open --project "review" --cwd "$PWD" --title "review" --json -- bash -lc 'make test') || exit 1
tab=$(printf '%s' "$out" | jq -er .tab.id) || exit 1
```

Prints `{"project","tab","created"}`, with or without `--json`. See
[`cli.md#open`](../reference/cli.md#open) for `--hold`, `--focus`, and
its refusals.

### `wait`

Block until a tab reaches a condition — the no-`sleep` synchronization
primitive:

```bash roost-recipe
roostctl wait --tab "$ROOST_TAB_ID" --text 'BUILD OK' --timeout 60
```

`--state`, `--text`, and `--gone` are the three conditions (at least
one required; all given must hold). See [`cli.md#wait`](../reference/cli.md#wait)
for the full condition/flag table, including `--no-timeout` for an
unbounded wait and `--interval-ms`.

### `events`

Print the event stream, one JSON line per event, until it ends —
useful for watching a tab from outside rather than polling it:

```bash roost-recipe
timeout 10 roostctl events --tab "$ROOST_TAB_ID" | jq -c 'select(.event == "notification.fired")' || true
```

See [`cli.md#events`](../reference/cli.md#events) for the envelope
shape and exit codes.

### `rpc`

Call an operation by name when no verb covers it yet — the escape
hatch, not the everyday path:

```bash roost-recipe
roostctl rpc tab.dump '{"tab_id":"'"$ROOST_TAB_ID"'","scrollback":50}'
```

`rpc` bypasses the target policy below and does not read
`ROOST_TAB_ID` — see [`cli.md#rpc`](../reference/cli.md#rpc). It is
not a way around `--tab` for a verb that has one: use the named verb
for anything a verb covers.

## Waiting from a raw socket (not `roostctl`)

A client that isn't `roostctl` and wants to follow events instead of
polling — say, a language with its own JSON socket client — needs two
connections, because a connection that has called `events.subscribe`
answers nothing else:

1. Pick the socket: `identify.local_session_socket` if present, else
   the UI socket if `identify.ops` lists `events.subscribe`, else
   there's no stream and you poll `tab.list` instead.
2. Subscribe on connection **A**. Keep the ack's `revision` and
   `session_id`.
3. On connection **B**, check identity (`identify.instance_id` on a UI
   socket, `session.identify.session_id` on a session socket) against
   the ack's `session_id`, then snapshot with `tab.list`.
4. Discard every batch on A whose `revision` is `<=` the snapshot's;
   apply the rest in order.

The full recipe, including every refusal and how to resume a dropped
stream, is [`ipc.md`'s "Waiting on a
condition"](../reference/ipc.md#waiting-on-a-condition) — this is a
pointer to it, not a restatement; `roostctl wait` and `roostctl
events` already implement it for you.

## The focus rule

Never pass `--focus` or run `tab focus` unless the user (or the task)
actually asked to switch tabs — it raises the window and steals
attention from whatever the person is doing. This matters independent
of `open`/`tab open`'s own behavior: opening a tab makes it the active
tab regardless of `--focus` (#503), so `--focus` only changes whether
the window is raised and switched to, not whether the new tab becomes
the active one in the sidebar.

## Target policy: which tab a command acts on

A command that **changes** a tab — `notify`, `set-title`, `tab
set-state`, `tab clear-notification`, `tab close`, `tab send`, `tab
resize`, `tab focus` — needs `--tab`, or else `$ROOST_TAB_ID`. With
neither, it refuses before dialling anything and exits 2 `usage`: the
UI's active tab is whatever a person last clicked, which is no answer
for a command that writes to it. Every shell inside a Roost tab has
`ROOST_TAB_ID` already set to its own tab, so the bare form works
there; outside one — a CI runner, a second terminal, an agent driving
Roost from elsewhere — pass `--tab` explicitly. The read-only `tab
dump` and `wait` still fall back to the UI's active tab when
`ROOST_TAB_ID` is unset. See
[`cli.md`'s "Which tab a command acts on"](../reference/cli.md#which-tab-a-command-acts-on)
for the exact list and the refusal text.

## Exit codes

Every failure prints one line on stderr — `roostctl: <code>: <message>`,
or `{"error":{"code","message"}}` under `--json` — and `code` is the
stable part to branch on:

| Exit | `code` | Meaning |
|---|---|---|
| 0 | — | Success |
| 2 | `usage` | A bad command line, including a command that changes a tab given no `--tab` and no `ROOST_TAB_ID` |
| 1 | `no-target` | Auto-detect found nothing listening at any known socket |
| 1 | `ambiguous-target` | Several Roost UIs are running; pass `--target` |
| 1 | `connection` | Dialing, reading or writing the socket failed, or the stream dropped |
| 1 | *the server's own* | The server refused the request; its code is passed through verbatim (`not-found`, `invalid-param`, `unknown-op`, …) |
| 1 | `unsupported` | This server does not serve an op the command needs |
| 1 | `checks-failed` | `doctor` found a failing check |
| 1 | `failed` | Something on this machine outside the wire: a file, a binary, an unset `$HOME` |
| 3 | `not-running` | `session status` found no session running |
| 4 | `timeout` | `wait`'s condition did not hold before `--timeout` |

The authoritative version of this table, including the partial-failure
exception for the `agent` verbs, is
[`cli.md#exit-codes`](../reference/cli.md#exit-codes).

## The Mac app

The Swift `Roost.app` omits `identify.ops` entirely, which every verb
above treats the same as an `ops` list missing the op it needs
(#510):

- `open` and `project ensure` refuse `unsupported` — use `project
  list --json` to find a project by name, then `tab open
  --project-id`.
- `wait` falls back to polling `tab.list` (and `tab.dump` for
  `--text`) at `--interval-ms` instead of following the event stream.
- `events` fails outright with the server's own refusal (today,
  `not-implemented`) — there's no poll fallback for a continuous
  stream.

Everything else in this guide — `identify`, the target policy, `tab
send`/`tab dump`/`notify`, the exit-code table — works the same on
both platforms.

## The agent skill

If you're driving Roost from inside a coding agent rather than an
ad-hoc script, [Agent Skill](agent-skill.md) covers the packaged skill
that teaches an agent the same rules this guide documents, and how to
install it.
