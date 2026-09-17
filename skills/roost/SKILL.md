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

If it fails, say that Roost is not running or not reachable from here, and stop. Do not start Roost, and do not guess socket paths to get around the failure.

Inside a Roost tab, `ROOST_TAB_ID` and `ROOST_SOCKET` are set. `ROOST_TAB_ID` is **your own tab**, the one this agent runs in:

```bash
printf '%s\n' "${ROOST_TAB_ID:-not in a Roost tab}"
```

A verb that takes `--tab` falls back to `ROOST_TAB_ID` when you leave the flag out, so inside Roost an omitted `--tab` means your own tab. `events` is the exception: its `--tab` is only a filter.

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

Do not bake syntax from memory or from the examples below; read `--help` before a verb's first use. Never probe a verb by leaving out its arguments: `open` and `project ensure` execute with their defaults (`--cwd` is `$PWD`) and create a project, and `open` a tab.

Every verb accepts `--json`, before or after the verb, and then prints its result as JSON on stdout. A failure goes to stderr as `{"error":{"code","message"}}` under `--json`, and as `roostctl: <code>: <message>` without it. `open`, `project ensure`, `events`, and `rpc` always print JSON.

The full reference is <https://charliek.github.io/roost/reference/cli/>.

## Understand projects, tabs, and state

- A **project** is a named working directory in the sidebar, and it holds **tabs**. A tab is one terminal running one shell or command. Roost has one window and no splits or panes.
- Every tab has a `state`: `none`, `running`, `needs_input`, or `idle`. A plain shell is `running` while a foreground command runs and `none` at its prompt. An agent whose hooks report to Roost drives the state itself: `running` during its turn, `needs_input` when it waits on the user or failed, `idle` when its turn ended. `roostctl agent status` shows which agents have hooks installed.
- Tab and project ids are integers, written as strings in JSON (`"7"`). A tab on a connected remote host is `h<host>.<id>` (`h2.7`); `--help` says which verbs take that form.
- Ids belong to the running Roost. A restart, or a switch of its local backend, gives every tab a new id, so re-read ids after either.
- `roostctl identify --json` lists `ops`, the operations this Roost serves right now. If `ops` is missing or lacks `project.ensure` (the macOS app today), `open` and `project ensure` refuse with `unsupported`; the manual route is `project list --json` and then `tab open --project-id`. If `ops` lacks `events.subscribe` and there is no `local_session_socket`, `wait` polls instead of following events, and `events` fails.
- `ops` can also list seams that exist for Roost's own UI and test suite: the `app.*` family, `tab.feed_*`, `tab.capture_pty_input`, and more under `ROOST_TEST_MODE=1`. Never call them.

## Recipes

The recipes use placeholders: `X` is a project name, `N` is a tab id you parsed from JSON output, and `"$PWD"` is the directory to work in. Substitute real values before running a line.

### Open a tab that runs a command

Only do this when the user asked for something to run in Roost. `open` finds the project named `X`, or creates it at `--cwd`, then opens a tab in it running the command after `--`. It prints `{"project","tab","created"}`; take the new tab's id from `.tab.id`:

```bash roost-recipe
tab=$(roostctl open --project X --cwd "$PWD" --title tests --hold --json -- sh -c 'make test; echo "make exited $?"' | jq -r .tab.id)
roostctl wait --tab "$tab" --text 'make exited' --timeout 600
roostctl tab dump --tab "$tab" --scrollback 200
```

Without `--hold` the tab closes as soon as the command exits, taking its output with it. With `--hold` an interactive shell takes over afterwards and the output stays on screen. Without a command, the tab opens the default shell.

### Send input to a tab and wait

A tab running a shell shows the command you type, so wait on text that only the command's output contains. The quotes below split `make exited` in the typed line, and the shell joins it in the output:

```bash roost-recipe
roostctl tab send --tab N --bytes 'make test; echo "make "exited $?\n'
roostctl wait --tab N --text 'make exited' --timeout 600
```

`--bytes` decodes escapes: `\r` is Enter (a shell also takes `\n`), `\x1b` is Escape. For an agent running in the tab, send the prompt and then Enter as a separate write, so an input box that detects pastes does not take Enter as part of the pasted text. An agent that finished its last turn is already `idle`, so wait for its turn to start before waiting for it to end:

```bash roost-recipe
roostctl tab send --tab N --bytes 'Summarize the failing tests in one paragraph.'
roostctl tab send --tab N --bytes '\r'
roostctl wait --tab N --state running --timeout 30
roostctl wait --tab N --state idle --timeout 120
```

`wait` exits 0 once the condition holds and 4 (`timeout`) if it does not in time. If the `running` wait times out, the turn may already have started and ended, or the prompt never submitted. A turn that stops on a permission prompt is `needs_input`, never `idle`, so an `idle` wait on it times out: read the tab before deciding what to send. `--no-timeout` waits for as long as it takes; only use it when the user asked you to wait indefinitely.

### Read what happened

`tab dump` prints the tab's screen, one line per row; `--scrollback` adds that many history rows above it, and `--json` adds the size and cursor. `tab list --json` holds every project and tab with its `state`:

```bash roost-recipe
roostctl tab dump --tab N --scrollback 200
roostctl tab list --json | jq --arg id N '.projects[].tabs[] | select(.id == $id) | {title, state, has_notification}'
```

### Watch for attention

`events` prints one JSON line per event until the stream ends, which may be never, so always bound it with `timeout`. A `tab.state_changed` event with `data.state` of `needs_input` means the tab's agent is waiting on the user; `notification.fired` is a notification raised on the tab:

```bash roost-recipe
timeout 30 roostctl events --tab N | jq -c 'select(.event == "notification.fired" or .data.state == "needs_input")'
```

To block until one tab needs input, `roostctl wait --tab N --state needs_input` is simpler. `events` exits 0 when the stream ends, which it does when Roost switches its local backend or a session stops.

### Tell the user

`notify` raises a notification on a tab. Put it on the tab the news is about, usually your own:

```bash roost-recipe
roostctl notify --tab N --title 'Tests finished' --body 'make test exited 0'
roostctl notify --tab "$ROOST_TAB_ID" --title 'Review ready' --body 'Findings are in REVIEW.md'
```

### Call an op no verb covers

`rpc` calls any operation by name, with its params as a JSON object, and prints the result as JSON. Check `identify --json`'s `ops` first; the wire format is at <https://charliek.github.io/roost/reference/ipc/>:

```bash roost-recipe
roostctl rpc tab.list
roostctl rpc tab.dump '{"tab_id":"N","scrollback":50}'
```

## Rules

- Always pass `--tab`: without it, `notify`, `set-title`, `tab set-state`, `tab clear-notification`, `tab close`, `tab send`, `tab resize`, and `tab focus` act on `ROOST_TAB_ID`, which inside Roost is your own tab, and refuse with exit 2 outside Roost, while `tab dump` and `wait` fall back to whichever tab the user last clicked.
- Parse ids from `--json` output. Never derive them from sidebar order, tab titles, or examples.
- Never pass `--focus` or run `tab focus` unless the user asked to switch tabs. Opening a tab makes it the active tab even without `--focus`, so only open tabs when the user asked for something to run in Roost.
- Never close a tab or delete a project you did not open, unless the user explicitly asked.
- A `wait` timeout does not prove the input was not delivered or the command did not run. Read the tab with `tab dump` before sending anything again.
- `rpc` is not a way around `--tab`. Use the named verb for anything a verb covers, and only put ids in `rpc` params that you parsed from JSON.
- Never call the test seams `identify` may list (`app.*`, `tab.feed_*`, `tab.capture_pty_input`).
- Do not run `roostctl session stop`, `roostctl agent install`, or `roostctl agent uninstall` unless the user asked for exactly that.
- Branch on the exit code and the error `code`, never on the message. Exit 0 is success; every failure is one of these:

| Exit | `code` | Meaning |
|---|---|---|
| 2 | `usage` | A bad command line, including a tab-changing verb given no `--tab` and no `ROOST_TAB_ID` |
| 1 | `no-target` | No Roost is listening at any known socket |
| 1 | `ambiguous-target` | Several Roost UIs are running; pass `--target` |
| 1 | `connection` | The socket failed or the stream dropped; a `wait` across a restart or a backend switch ends here, so re-read ids |
| 1 | the server's own | The server refused (`not-found`, `invalid-param`, `unknown-op`, …); its code is passed through verbatim |
| 1 | `unsupported` | This Roost does not serve an op the verb needs, such as `open` on the macOS app |
| 1 | `checks-failed` | `roostctl doctor` found a failing check |
| 1 | `failed` | Something local outside the socket failed: a file, a binary, `$HOME` |
| 3 | `not-running` | `roostctl session status` found no session running |
| 4 | `timeout` | `wait`'s condition did not hold before `--timeout` |
