---
name: roost
description: "Drive Roost, the desktop terminal multiplexer for coding agents, through its roostctl CLI. Use only when the user mentions Roost or asks to open, inspect, drive, or wait on Roost tabs or projects. Do not use merely because a task could benefit from a background terminal, a second shell, or parallel work. Requires a running Roost: `roostctl identify` must answer."
---

# Roost

Roost is a desktop terminal multiplexer for coding agents: a sidebar of projects, tabs in each project, one terminal per tab. `roostctl` talks to the running Roost over a local socket. Use it to open tabs, send input, read a tab's screen, wait on a tab's state, and notify the user.

Before any other command, check that a Roost answers:

```bash
roostctl identify --json
```

If it fails, report the error `code` it printed and stop. `no-target` means no Roost is running or reachable from here. `ambiguous-target` means several Roost UIs are running and the user has to say which one; then pass it as `--target` (`mac`, `linux`, or `iced`) on every command. Do not start Roost, and do not guess socket paths to get around a failure.

Inside a Roost tab, `ROOST_TAB_ID` and `ROOST_SOCKET` are set. `ROOST_TAB_ID` is **your own tab**, the one this agent runs in:

```bash
printf '%s\n' "${ROOST_TAB_ID:-not in a Roost tab}"
```

A verb that takes `--tab` falls back to `ROOST_TAB_ID` when you leave the flag out, so inside Roost an omitted `--tab` means your own tab. Two verbs never read `ROOST_TAB_ID`: `events`, whose `--tab` is only a filter, and `tab send-file`, which exits 2 without an explicit `--tab`.

## Learn the current CLI

The installed binary is the authority for command syntax. Start with:

```bash
roostctl --help
```

Then print the help of the group or verb you need:

```bash
roostctl tab --help
roostctl project --help
roostctl open --help
roostctl wait --help
roostctl events --help
roostctl notify --help
roostctl rpc --help
```

Do not bake syntax from memory or from the examples below; read `--help` before a verb's first use. Never probe a verb by running it with only its required flags: `open --project X` and `project ensure --name X` fill in the rest (`--cwd` is `$PWD`) and create the project if none has that name, and `open` also opens a tab.

Every verb accepts `--json`, before or after the verb, and then prints its result as JSON on stdout. A failure goes to stderr as `{"error":{"code","message"}}` under `--json`, and as `roostctl: <code>: <message>` without it. `open`, `project ensure`, `events`, and `rpc` always print JSON. The hook-writing verbs `agent ensure`, `agent set`, `agent install`, and `agent uninstall` differ: when part of their work fails they exit 1 with their report on stdout and no error envelope.

The full reference is <https://charliek.github.io/roost/reference/cli/>.
The automation guide is <https://charliek.github.io/roost/guides/automation/>.

## Understand projects, tabs, and state

- A **project** is a named working directory in the sidebar, and it holds **tabs**. A tab is one terminal running one shell or command. Roost has one window and no splits or panes.
- Every tab has a `state`: `none`, `running`, `needs_input`, or `idle`. A plain shell sets it only through the OSC 133 prompt marks of shell integration, which Roost loads for zsh and bash 4.4 or newer: `running` while a foreground command runs, `none` at its prompt. A shell without the marks stays `none` while work runs (its `shell_state` in `tab list --json` stays `unknown`), so wait on a shell's output text, not its state. An agent whose hooks report to Roost drives the state itself: `running` during its turn, `needs_input` when it waits on the user or its turn failed, `idle` when its turn ended. `roostctl agent status` shows which agents have hooks installed.
- Tab and project ids are integers, written as strings in JSON (`"7"`). A tab on a connected remote host is `h<host>.<id>` (`h2.7`); `--help` says which verbs take that form.
- Ids belong to the running Roost. A restart, or a switch of its local backend, gives every tab a new id, so re-read ids after either.
- `roostctl identify --json` lists `ops`, the operations this Roost serves right now. If `ops` is missing or lacks `project.ensure` (the Swift `Roost.app`; Roost-Iced running on macOS is not this and serves the full surface below), `open` and `project ensure` refuse with `unsupported`; the manual route is `project list --json` and then `tab open --project-id`. If `ops` lacks `events.subscribe` and there is no `local_session_socket`, `wait` polls instead of following events, and `events` fails. A session's own `identify` carries no `instance_id`; its identity is `session_id`.
- `ops` can also list seams that exist for Roost's own UI and test suite: the `app.*` family, `tab.feed_*`, `tab.capture_pty_input`, and more under `ROOST_TEST_MODE=1`. Never call them.

## Recipes

The recipes run in bash and need `jq`; watching events also needs `timeout` from GNU coreutils, which stock macOS lacks. They use placeholders: `X` is a project name, `N` is a tab id you parsed from JSON output, and `"$PWD"` is the directory to work in. Substitute real values before running a line.

### Open a tab that runs a command

Only do this when the user asked for something to run in Roost. `open` finds the project named `X`, or creates it at `--cwd`, then opens a tab in it running the command after `--`. It prints `{"project","tab","created"}`. Keep that output and check the exit before taking the new tab's id from `.tab.id`, because a plain `jq` also succeeds on the empty output of a failed `open`:

```bash roost-recipe
out=$(roostctl open --project X --cwd "$PWD" --title tests --hold --no-activate --json -- sh -c 'make test; echo "make exited $?"') || exit 1
tab=$(printf '%s' "$out" | jq -er .tab.id) || exit 1
roostctl wait --tab "$tab" --text 'make exited' --timeout 600
roostctl tab dump --tab "$tab" --scrollback 200
```

Without `--hold` the tab closes as soon as the command exits, taking its output with it. With `--hold` an interactive shell takes over afterwards and the output stays on screen. Without a command, the tab opens the default shell.

### Send input to a tab and wait

A tab running a shell shows the line you type, and `wait --text` matches anywhere on the tab's current screen, a previous run's output included. So end the command with a marker unique to this run, split by quotes in the typed line so that only the output joins it. `sh -c` keeps the typed syntax the same whichever shell the tab runs, fish included:

```bash roost-recipe
run=$RANDOM$RANDOM
roostctl tab send --tab N --bytes "sh -c 'make test; echo \"make-done-\"$run \$?'\n"
roostctl wait --tab N --text "make-done-$run" --timeout 600
```

`--bytes` decodes escapes: `\r` is Enter (a shell also takes `\n`), `\x1b` is Escape. `--no-timeout` waits for as long as it takes; only use it when the user asked you to wait indefinitely.

### Prompt an agent and wait for its turn

`tab prompt` writes the prompt, then Enter as a separate write, then waits behind an activity gate before waiting for the turn to settle — one verb closing the race a `tab send` followed by a separate `wait --state running` can miss:

```bash roost-recipe
roostctl tab prompt --tab N --timeout 600 'Summarize the failing tests in one paragraph.'
```

`--timeout` is required unless you pass `--no-timeout`: a turn's length is not something this verb can guess. Two caveats. The gate is **temporal, not causal**: it only checks that the tab reached `running` after the prompt, not that this prompt is what caused it. And a tab whose agent reports nothing to Roost — no hooks installed, and not one of the agents that report directly — never reaches `running`, so the gate times out by design — exit 4 `stalled` still means the text and Enter *were* written, so read the tab with `tab dump` before sending anything again.

### Read what happened

`tab dump` prints the tab's screen, one line per row; `--scrollback` adds that many history rows above it, and `--json` adds the size and cursor. `tab list --json` holds every project and tab with its `state`:

```bash roost-recipe
roostctl tab dump --tab N --scrollback 200
roostctl tab list --json | jq --arg id N '.projects[].tabs[] | select(.id == $id) | {title, state, has_notification}'
```

### Watch for attention

`events` prints one JSON line per event until the stream ends, which may be never, so always bound it with `timeout`. A `tab.state_changed` event with `data.state` of `needs_input` means the tab's agent is waiting on the user or its turn failed, so read the tab before responding; `notification.fired` is a notification raised on the tab:

```bash roost-recipe
set -o pipefail
timeout 30 roostctl events --tab N | jq -c 'select(.event == "notification.fired" or (.event == "tab.state_changed" and .data.state == "needs_input"))'
```

With `pipefail` the exit tells the endings apart: 124 when `timeout` stopped the stream, 0 when the stream ended on its own (Roost switched its local backend, or a session stopped), and `roostctl`'s own code when it failed. Where `timeout` is missing, or to block until one tab needs input, `roostctl wait --tab N --state needs_input --timeout 300` is the portable bound; `wait` gives up after 5 seconds when you leave out `--timeout`.

### Tell the user

`notify` raises a notification on a tab. Put it on the tab the news is about, usually your own, which inside a Roost tab is `$ROOST_TAB_ID`. Outside Roost that variable is unset, so the second line passes an empty `--tab` and exits 2:

```bash roost-recipe
roostctl notify --tab N --title 'Tests finished' --body 'make test exited 0'
roostctl notify --tab "$ROOST_TAB_ID" --title 'Review ready' --body 'Findings are in REVIEW.md'
```

### Call an op no verb covers

`rpc` calls an operation by name, with its params as a JSON object, and prints the result as JSON. It is for an op that `identify --json`'s `ops` lists and this `roostctl` has no verb for, such as one a newer Roost added. Today every op an agent needs has a verb, so prefer the verb. The line below only illustrates the call's shape, on an op the `identify` verb already covers; the wire format is at <https://charliek.github.io/roost/reference/ipc/>:

```bash roost-recipe
roostctl rpc identify '{}'
```

`roostctl schema` prints the pinned JSON Schema for every op and event, with no socket needed — check a field's shape there before an `rpc` call or before hand-crafting raw-socket traffic.

## Rules

- Always pass `--tab`. Without it, `notify`, `set-title`, `tab set-state`, `tab clear-notification`, `tab close`, `tab send`, `tab resize`, `tab focus`, `tab report`, and `tab prompt` act on `ROOST_TAB_ID`, which inside Roost is your own tab, and exit 2 when it is unset or empty. `tab dump` and `wait` also use `ROOST_TAB_ID` first, but when it is unset or empty they fall back to the UI's active tab, whichever tab the user last clicked.
- Parse ids from `--json` output. Never derive them from sidebar order, tab titles, or examples.
- Never pass `--focus` or run `tab focus` unless the user asked to switch tabs. Opening a tab with `--no-activate` leaves the user's selection where it was; without it, opening may select the tab. A server that does not know the flag (the Swift `Roost.app`, not Roost-Iced on macOS) refuses the whole command — `unknown-field` from `tab open`, `unsupported` from `open` — and opens no tab at all, so re-read `tab list --json` rather than assuming one exists.
- Never close a tab or delete a project you did not open, unless the user explicitly asked.
- A `wait` timeout does not prove the input was not delivered or the command did not run. Read the tab with `tab dump` before sending anything again.
- `rpc` is not a way around `--tab`. Use the named verb for anything a verb covers, and only put ids in `rpc` params that you parsed from JSON.
- Never call the test seams `identify` may list (`app.*`, `tab.feed_*`, `tab.capture_pty_input`).
- Do not run `roostctl session stop`, `roostctl agent install`, or `roostctl agent uninstall` unless the user asked for exactly that.
- `busy` is the one server code worth retrying: it means a local-backend switch is in flight. An older Roost without `busy` refuses the same switch as `host-unavailable` instead; when `identify --json` still shows `local_backend_switch`, treat that the same as `busy`. `roostctl open` refuses a switch in flight as `usage` (exit 2), which otherwise means a bad command line: treat an `open` `usage` as retryable only when `identify --json` read straight after it still shows `local_backend_switch`. `wait`, `tab prompt`, and `events` already hold off on both refusals themselves, so this retry rule is for every other verb: poll `identify --json` until `local_backend_switch` is absent, then retry, re-reading any tab or project id first.
- Branch on the exit code and the error `code`, never on the message. Exit 0 is success; every failure that prints an error envelope is one of these (the `agent` verbs' partial failure above prints none):

| Exit | `code` | Meaning |
|---|---|---|
| 2 | `usage` | A bad command line, including a tab-changing verb given no `--tab` and no `ROOST_TAB_ID` |
| 1 | `no-target` | No Roost is listening at any known socket |
| 1 | `ambiguous-target` | Several Roost UIs are running; pass `--target` |
| 1 | `connection` | The socket failed or the stream dropped, or a call went unanswered for 30s under `--no-timeout`; a `wait` across a restart, a backend switch, or a session stop ends here, so re-read ids |
| 1 | the server's own | The server refused (`not-found`, `invalid-param`, `unknown-op`, `busy`, …); its code is passed through verbatim |
| 1 | `unsupported` | This Roost does not serve an op the verb needs, such as `open` on the Swift `Roost.app` (Roost-Iced on macOS serves it) |
| 1 | `checks-failed` | `roostctl doctor` found a failing check |
| 1 | `failed` | Something local outside the socket failed: a file, a binary, `$HOME` |
| 3 | `not-running` | `roostctl session status` found no session running |
| 4 | `timeout` | `wait`'s or `tab prompt`'s condition did not hold before `--timeout` |
| 4 | `stalled` | `tab prompt` wrote the prompt, but the tab reached no `running` within `--activity-timeout` |
