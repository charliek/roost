# Extending Roost

Roost is built so that **everything routes through one operation set** —
the same ops the UI buttons, hotkeys, and `roostctl` drive. That makes the
app scriptable from the outside without any plugin runtime: if you can
open a Unix socket (or run `roostctl`), you can drive Roost.

This guide is for **users who want to automate or extend Roost** — wiring
up custom commands, dynamic menus, and scripts. There are three layers,
from simplest to most powerful:

1. **[`roostctl` / the IPC socket](#1-drive-roost-from-a-script)** — script
   anything the app can do.
2. **[The command launcher](#2-the-command-launcher) (`command =`)** —
   pin fixed commands to a picker (⌘⇧T / Alt+Shift+T).
3. **[Dynamic providers](#3-dynamic-providers) (`provider =`)** —
   a script that *generates* a menu on demand and acts on your choice
   (⌘⇧E / Alt+Shift+E).

All three are local and trusted — the same trust level as your shell rc.
Roost does not run anything from an untrusted source.

---

## 1. Drive Roost from a script

A running Roost serves a newline-delimited JSON socket. `roostctl` is a
thin CLI over it; anything `roostctl` does, your script can do directly.

```bash
# Find a project named "review", create it if missing, open a tab, run a command.
name="review"
pid=$(roostctl tab list --json | jq -r --arg n "$name" \
        '.projects[] | select(.name==$n) | .id' | head -1)
[ -z "$pid" ] && pid=$(roostctl project create --name "$name" --cwd "$PWD" \
        | jq -r '.project.id')
roostctl tab open --project-id "$pid" --title "$name" -- bash -lc 'make test'
```

A script launched *inside* a Roost tab can call back without any discovery
— Roost injects `ROOST_SOCKET` and `ROOST_TAB_ID` into every tab's
environment:

```bash
roostctl --socket "$ROOST_SOCKET" set-title --tab "$ROOST_TAB_ID" --title "building…"
```

See the [CLI reference](../reference/cli.md) and the
[IPC wire format](../reference/ipc.md) for the full op set.

---

## 2. The command launcher

Add fixed commands to `~/.config/roost/config.conf`; they show up in the
command launcher (⌘⇧T on macOS, Alt+Shift+T on Linux) and run in a new
tab when selected.

```ini
command = label="Lazygit" run="lazygit"
command = label="Logs"    run="docker compose logs -f" hold=true
command = label="Claude"  run="claude --resume" env="ANTHROPIC_LOG=debug"
```

| Key | Meaning |
|---|---|
| `label` | Row title (required). |
| `run` | Command, run through your login shell (required). |
| `title` | Tab title (defaults to `label`). |
| `hold` | `true` keeps the shell open after `run` exits, so output stays on screen. |
| `env` | `K=V` pairs exported before `run`. |

---

## 3. Dynamic providers

A **provider** is a script Roost runs to *build a menu on demand*, then
runs again when you pick a row. Where `command =` launches a fixed
command, a provider produces a **dynamic list** — "open one of my shed
services", "switch to a worktree", "resume a saved session" — and then
acts on the choice (which can drive Roost via `$ROOST_SOCKET`, or do
something else entirely).

Providers appear in the **custom palette** (⌘⇧E / Alt+Shift+E), and as a
**"Custom Commands…"** row in the command palette (⌘⇧P) whenever at least
one is configured.

### Registering a provider

Two ways, which merge (config entries first, then the directory):

```ini
# In ~/.config/roost/config.conf — same grammar as `command =`:
provider = label="Open shed" run="~/.config/roost/providers/shed.sh" timeout=5 limit=100
```

```text
# …or drop an executable in the providers directory beside the config:
~/.config/roost/providers/shed.sh      # chmod +x; filename → label
```

For a directory entry, the label defaults to a humanized filename; add a
header comment to override:

```bash
#!/usr/bin/env bash
# @roost.label: Open shed
# @roost.title: Pick a service
```

| Key (config form) | Meaning |
|---|---|
| `label` / `run` | Required, as for `command =`. |
| `title` | Sub-menu placeholder (defaults to `label`). |
| `timeout` | Seconds before Roost kills a hung run (default `5`). |
| `limit` | Max rows a single `list` may contribute (default `100`). |

### The contract

Roost runs your `run` command **twice**, distinguished by an argv phase
(`$1`) and the `ROOST_PROVIDER_PHASE` env var:

- **`list`** — print the rows to populate the menu.
- **`activate`** — run after the user picks a row; `ROOST_SELECTED_ID`
  holds the chosen row's `id`.

**Input.** Each run gets the active-tab context two ways — pick whichever
is convenient:

- **Env vars:** `ROOST_SOCKET`, `ROOST_PROVIDER_PHASE`, `ROOST_QUERY`,
  `ROOST_ACTIVE_TAB_ID`, `ROOST_ACTIVE_PROJECT_ID`, `ROOST_ACTIVE_CWD`,
  `ROOST_ACTIVE_TITLE`, and on activate `ROOST_SELECTED_ID`.
- **Stdin JSON:**
  ```json
  { "v": 1, "phase": "activate", "selected_id": "api", "query": "ap",
    "active_tab": {"id": "7", "project_id": "3", "cwd": "/repo", "title": "build"},
    "socket": "/Users/you/Library/Caches/Roost/roost.sock" }
  ```

**Output (stdout, JSON).** Both phases may print:

```json
{ "placeholder": "Pick a service",
  "items": [ {"id": "web", "title": "shed: web", "subtitle": "../shed/web"},
             {"id": "api", "title": "shed: api"} ] }
```

A bare `[ … ]` array is also accepted. On **`activate`**:

- print **nothing** (or `{}`) → the palette closes (your script already
  did its work);
- print **more `items`** → Roost drills into a sub-menu — the same schema
  as `list`, so multi-step menus need no extra mechanism.

**Safety rails.** stdout must be valid JSON, so don't let your shell rc
echo onto it (Roost runs `run` non-interactively). Rows past `limit` are
dropped with a "… N more" hint; a run that exceeds `timeout`, exits
non-zero, or prints garbage surfaces an error row (and is logged).
`limit` bounds *rows*, not output size — a provider stuck printing is
bounded by `timeout` (killed when it elapses, escalating to `SIGKILL`).
Discovered scripts are run by absolute (shell-quoted) path; a config
`run =` string is shell-interpreted, exactly like `command =`.

### Examples

Because input is env-or-stdin and output is "print one JSON object", a
provider is a few lines in any language:

=== "bash"

    ```bash
    #!/usr/bin/env bash
    # @roost.label: Open shed
    case "$1" in
      list)
        printf '{"items":[{"id":"web","title":"shed: web"},{"id":"api","title":"shed: api"}]}' ;;
      activate)
        roostctl --socket "$ROOST_SOCKET" tab open \
          --project-id "$ROOST_ACTIVE_PROJECT_ID" -- shed open "$ROOST_SELECTED_ID" ;;
    esac
    ```

=== "python"

    ```python
    #!/usr/bin/env python3
    import json, os, subprocess, sys
    inp = json.load(sys.stdin)
    if inp["phase"] == "list":
        json.dump({"items": [{"id": s, "title": f"shed: {s}"} for s in ("web", "api")]}, sys.stdout)
    else:
        subprocess.run(["roostctl", "--socket", inp["socket"], "tab", "open",
                        "--project-id", inp["active_tab"]["project_id"],
                        "--", "shed", "open", inp["selected_id"]])
    ```

=== "typescript"

    ```ts
    #!/usr/bin/env -S node
    const inp = JSON.parse(require("fs").readFileSync(0, "utf8"));
    if (inp.phase === "list") {
      process.stdout.write(JSON.stringify({ items: ["web", "api"].map(s => ({ id: s, title: `shed: ${s}` })) }));
    } else {
      require("child_process").execFileSync("roostctl",
        ["--socket", inp.socket, "tab", "open", "--project-id", inp.active_tab.project_id,
         "--", "shed", "open", inp.selected_id]);
    }
    ```

---

## `palette.present` — let a script drive its own menu

If you'd rather your script own the whole flow (gather options, show a
menu, act), use `palette.present`: hand Roost a list, get back the choice.
It blocks until the user picks or dismisses.

```bash
items='[{"id":"web","title":"shed: web"},{"id":"api","title":"shed: api"}]'
choice=$(roostctl palette present --title "Open shed" --items "$items" --json | jq -r .selected_id)
[ -n "$choice" ] && shed open "$choice"
```

Items can also be piped on stdin (`… | roostctl palette present`). This is
the same item schema providers print — a provider is just the
Roost-driven version of the same contract.
