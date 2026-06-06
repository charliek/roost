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
  `ROOST_ACTIVE_TITLE`, `ROOST_ROOSTCTL` (absolute path to Roost's own
  `roostctl` when it can resolve one — best-effort, so keep the
  `"${ROOST_ROOSTCTL:-roostctl}"` fallback; [see below](#opening-tabs-from-activate)),
  and on activate `ROOST_SELECTED_ID`.
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
             {"id": "api", "title": "shed: api"},
             {"id": "_none", "title": "No services", "actionable": false} ] }
```

Each item has a required `id` (round-trips back as `ROOST_SELECTED_ID`)
and `title`, an optional `subtitle`, and an optional **`actionable`**
(default `true`). Set `actionable: false` for an **empty/disabled row**
(e.g. "No results"): it renders but can't be selected, and selecting it is
a no-op that leaves the palette open — Roost never calls `activate` for it.

A bare `[ … ]` array is also accepted. On **`activate`**, stdout is
*optional* — it's read as a menu only when it looks like one:

- print **nothing** (or `{}`) → the palette closes (your script already
  did its work);
- print **more `items`** → Roost drills into a sub-menu — the same schema
  as `list`, so multi-step menus need no extra mechanism;
- print **anything else** → ignored; the palette closes. So a
  side-effecting command's incidental output (e.g. the new tab id
  `roostctl tab open` prints) is fine — **you don't need to redirect it**.
  (Output that *looks* like JSON — trimmed, starts with `{` or `[` — but
  doesn't parse is still reported as an error, so a malformed sub-menu
  isn't silently swallowed.)

### Opening tabs from `activate`

The usual thing `activate` does is open a tab that runs something, via
Roost's CLI. **Call `$ROOST_ROOSTCTL`, not a bare `roostctl`** — Roost
sets it to its own CLI so your provider works without `roostctl` on
`PATH`. Where that CLI lives differs by platform, which is exactly why
the env var exists:

| Platform | `roostctl` location | On `PATH`? |
|---|---|---|
| **Linux (`.deb`)** | `/usr/bin/roostctl` | ✅ yes |
| **macOS (`.dmg`)** | `Roost.app/Contents/Resources/bin/roostctl` (inside the bundle) | ❌ no — a Finder-launched app gets a minimal `PATH` |

`ROOST_ROOSTCTL` points at the right binary on both, so the portable form
is `"${ROOST_ROOSTCTL:-roostctl}"` (env var first, falling back to a
`PATH` copy for terminal/dev use). It runs `roostctl tab open`, which:

- `… tab open --project-id "$ROOST_ACTIVE_PROJECT_ID" -- <cmd>` runs `<cmd>`
  in a new tab that **closes when it exits** (hold=false);
- add `--hold` to keep the tab open afterward (drops to a shell);
- add `--after-tab "$ROOST_ACTIVE_TAB_ID"` to place it **next to the active
  tab**, and `--focus` to switch to it.

So "open this next to me and switch to it" is one call:
`"${ROOST_ROOSTCTL:-roostctl}" tab open --project-id … --after-tab … --focus -- <cmd>`.
See the [CLI reference](../reference/cli.md) for the full flag set.

**Safety rails.** A **`list`** must print valid JSON, so don't let your
shell rc echo onto it (Roost runs `run` non-interactively); **`activate`**
stdout is lenient (above). Rows past `limit` are dropped with a "… N more"
hint; a run that exceeds `timeout`, exits non-zero, or prints malformed
menu JSON surfaces an error row (and is logged).
`limit` bounds *rows*, not output size — a provider stuck printing is
bounded by `timeout` (killed when it elapses, escalating to `SIGKILL`).
Discovered scripts are run by absolute (shell-quoted) path; a config
`run =` string is shell-interpreted, exactly like `command =`.

### Examples

Because input is env-or-stdin and output is "print one JSON object", a
provider is a few lines in any language:

The bash tab is a complete **"Open shed"** provider: list the *running*
sheds, and on selection open `shed console <name>` in a tab next to the
current one (closing when you disconnect). Drop it at
`~/.config/roost/providers/shed.sh`, `chmod +x`.

=== "bash"

    ```bash
    #!/usr/bin/env bash
    # @roost.label: Open shed
    # Roost may launch with a minimal PATH, so cover where shed + jq live:
    # Homebrew (Mac), /usr/bin (Linux .deb), ~/.local/bin. Add other install
    # dirs as needed. (roostctl comes from $ROOST_ROOSTCTL, below.)
    export PATH="/opt/homebrew/bin:/usr/local/bin:/usr/bin:$HOME/.local/bin:$PATH"
    case "${1:-}" in
      list)
        rows=$(shed list --json 2>/dev/null \
          | jq -c '[.[] | select(.status=="running")
                   | {id: .name, title: ("shed: " + .name), subtitle: .ssh}]' || true)
        if [ -z "$rows" ] || [ "$rows" = "[]" ]; then
          printf '{"items":[{"id":"_none","title":"No running sheds","subtitle":"shed start <name>","actionable":false}]}'
        else
          printf '{"items":%s}' "$rows"
        fi ;;
      activate)
        # $ROOST_ROOSTCTL is Roost's own CLI (bundled off-PATH on Mac);
        # fall back to a PATH copy for terminal/dev runs. Login shell (-l)
        # for PATH; the shed name is a positional arg ($1), never spliced
        # into the script string. hold=false: when `shed console`
        # disconnects, the shell exits → the tab closes.
        "${ROOST_ROOSTCTL:-roostctl}" tab open --project-id "$ROOST_ACTIVE_PROJECT_ID" \
          --after-tab "$ROOST_ACTIVE_TAB_ID" --focus --title "shed: $ROOST_SELECTED_ID" \
          -- "${SHELL:-/bin/bash}" -lc 'shed console "$1"' shed "$ROOST_SELECTED_ID" ;;
    esac
    ```

=== "python"

    ```python
    #!/usr/bin/env python3
    import json, os, subprocess, sys
    # Minimal launch PATH? Cover where shed lives: Homebrew (Mac), /usr/bin
    # (Linux .deb), ~/.local/bin. Add other install dirs as needed.
    os.environ["PATH"] = f"/opt/homebrew/bin:/usr/local/bin:/usr/bin:{os.path.expanduser('~/.local/bin')}:{os.environ.get('PATH', '')}"
    inp = json.load(sys.stdin)
    if inp["phase"] == "list":
        sheds = json.loads(subprocess.run(["shed", "list", "--json"],
                                          capture_output=True, text=True).stdout or "[]")
        items = [{"id": s["name"], "title": f"shed: {s['name']}", "subtitle": s["ssh"]}
                 for s in sheds if s["status"] == "running"]
        if not items:
            items = [{"id": "_none", "title": "No running sheds", "actionable": False}]
        json.dump({"items": items}, sys.stdout)
    else:
        tab = inp["active_tab"]
        roostctl = os.environ.get("ROOST_ROOSTCTL", "roostctl")  # Roost's own CLI
        # Pass the shed name as a positional arg ($1), not interpolated.
        subprocess.run([roostctl, "tab", "open", "--project-id", tab["project_id"],
                        "--after-tab", tab["id"], "--focus",
                        "--", "/bin/bash", "-lc", 'shed console "$1"', "shed", inp["selected_id"]])
    ```

=== "typescript"

    ```ts
    #!/usr/bin/env -S node
    const { execFileSync } = require("child_process");
    // Minimal launch PATH? Cover where shed lives: Homebrew (Mac), /usr/bin
    // (Linux .deb), ~/.local/bin. Add other install dirs as needed.
    process.env.PATH = `/opt/homebrew/bin:/usr/local/bin:/usr/bin:${process.env.HOME}/.local/bin:${process.env.PATH ?? ""}`;
    const inp = JSON.parse(require("fs").readFileSync(0, "utf8"));
    if (inp.phase === "list") {
      const sheds = JSON.parse(execFileSync("shed", ["list", "--json"]).toString() || "[]");
      const items = sheds.filter((s: any) => s.status === "running")
        .map((s: any) => ({ id: s.name, title: `shed: ${s.name}`, subtitle: s.ssh }));
      process.stdout.write(JSON.stringify({
        items: items.length ? items : [{ id: "_none", title: "No running sheds", actionable: false }],
      }));
    } else {
      const t = inp.active_tab;
      const roostctl = process.env.ROOST_ROOSTCTL ?? "roostctl";  // Roost's own CLI
      // Shed name as a positional arg ($1), not interpolated into the script.
      execFileSync(roostctl, ["tab", "open", "--project-id", t.project_id,
        "--after-tab", t.id, "--focus", "--", "/bin/bash", "-lc", 'shed console "$1"', "shed", inp.selected_id]);
    }
    ```

### Advanced — multi-step wizards

`activate` is **recursive**: when it prints `{items}`, Roost drills into a
sub-frame, and picking a row there runs `activate` again with that row's
id. That's the whole mechanism behind a **wizard** — chain several prompts
with no new API. Three things make it work:

- **`list` is step 1; each `activate` is the next step.** Return more
  `items` to advance; print nothing to finish (the palette closes).
- **Carry state in the row id.** `activate` only ever receives the *one*
  selected id — there's no cross-step memory — so encode the choices so far
  into each row's id, a continuation token like `image=base&cpus=4&mem=8192`.
  The script is a state machine keyed on which keys the id already carries.
- **Run slow work in the new tab, not in `activate`.** `activate` has a
  ≤60s timeout, so anything lengthy (a build, a VM create) must be launched
  *inside* a tab you open — which also shows progress live. `activate`
  itself returns immediately.

The example below is a **"Create shed"** wizard: it requires a Git project
root (a non-selectable error row otherwise — see
[`actionable`](#the-contract)), walks image → CPUs → memory → disk, then
opens a tab that runs `shed create` (mounting the current directory via
`--local-dir`) and drops into the shed. The name is derived from the
folder, sanitized to alphanumerics, and de-duplicated. Context is read from
the `$ROOST_*` env vars ([above](#the-contract)); you could equally parse
the stdin JSON like the basic example. Drop it at
`~/.config/roost/providers/create-shed.py`, `chmod +x`.

```python
#!/usr/bin/env python3
# @roost.label: Create shed
"""Create-shed wizard: image → CPUs → memory → disk, then create a shed for
the current directory and open `shed console`. See the notes above for the
state-in-the-id and run-in-the-tab patterns."""
import json
import os
import re
import subprocess
import sys

# Roost may launch with a minimal PATH, so cover where `shed` lives: Homebrew
# (Mac), /usr/bin (Linux .deb), ~/.local/bin. Add other install dirs as needed.
# (roostctl comes from $ROOST_ROOSTCTL, so it needn't be on PATH.)
os.environ["PATH"] = f"/opt/homebrew/bin:/usr/local/bin:/usr/bin:{os.path.expanduser('~/.local/bin')}:{os.environ.get('PATH', '')}"

CPUS = [1, 2, 4, 8]
MEMS = [2048, 4096, 8192, 16384]  # MB
DISKS = ["10G", "20G", "50G"]


def emit(items):
    json.dump({"items": items}, sys.stdout)


def run(*args):
    """Run a command; return its stdout ('' on failure)."""
    try:
        p = subprocess.run(args, capture_output=True, text=True)
        return p.stdout if p.returncode == 0 else ""
    except OSError:
        return ""


def field(sel, key):
    """Pull a value out of an 'a=1&b=2' id."""
    for part in sel.split("&"):
        k, _, v = part.partition("=")
        if k == key:
            return v
    return ""


def shed_name(cwd):
    """basename → alphanumerics (fallback 'shed'), then -2/-3 on collision."""
    base = re.sub(r"[^A-Za-z0-9]", "", os.path.basename(cwd)) or "shed"
    try:
        taken = {s["name"] for s in json.loads(run("shed", "list", "--json") or "[]")}
    except (ValueError, KeyError, TypeError):
        taken = set()
    name, n = base, 2
    while name in taken:
        name, n = f"{base}-{n}", n + 1
    return name


# Step rows. `p` is the accumulated id prefix, folded into each new id.
def cpu_rows(p):
    return [{"id": f"{p}&cpus={c}", "title": f"{c} vCPU"} for c in CPUS]


def mem_rows(p):
    return [{"id": f"{p}&mem={m}", "title": f"{m // 1024} GB", "subtitle": f"{m} MB"} for m in MEMS]


def disk_rows(p):
    return [{"id": f"{p}&upper={d}", "title": f"{d} writable layer"} for d in DISKS]


def do_list(cwd):
    # Gate: only a Git project root (a .git entry; a subdir has none, so this
    # also enforces "at the root").
    if not os.path.exists(os.path.join(cwd, ".git")):
        emit([{"id": "_nogit", "title": "Project not found (no .git)",
               "subtitle": "create shed only works at a repo root", "actionable": False}])
        return
    try:
        imgs = json.loads(run("shed", "image", "ls", "--json") or "{}").get("images", [])
    except ValueError:
        imgs = []
    if not imgs:
        emit([{"id": "_none", "title": "No images", "actionable": False}])
    elif len(imgs) == 1:  # auto-skip the image step when there's only one
        emit(cpu_rows(f"image={imgs[0]['name']}"))
    else:
        emit([{"id": f"image={i['name']}", "title": i["name"],
               "subtitle": i.get("docker_ref", "")} for i in imgs])


def do_activate(cwd, sel):
    if "upper=" in sel:  # all four chosen → create in a new tab
        name = shed_name(cwd)
        roostctl = os.environ.get("ROOST_ROOSTCTL", "roostctl")
        shell = os.environ.get("SHELL", "/bin/bash")
        # Create + console run in the tab (see notes). On failure, drop to an
        # interactive shell so the error stays visible.
        inner = (
            'if shed create "$1" --local-dir "$2" --image "$3" '
            '--cpus "$4" --memory "$5" --upper-size "$6"; then\n'
            '  exec shed console "$1"\n'
            'else\n'
            '  echo; echo "shed create failed (see output above)."\n'
            '  exec "${SHELL:-/bin/bash}" -i\n'
            'fi'
        )
        subprocess.run([
            roostctl, "tab", "open",
            "--project-id", os.environ.get("ROOST_ACTIVE_PROJECT_ID", ""),
            "--after-tab", os.environ.get("ROOST_ACTIVE_TAB_ID", ""),
            "--focus", "--title", f"shed: {name}",
            "--", shell, "-lc", inner,
            "shed", name, cwd,
            field(sel, "image"), field(sel, "cpus"), field(sel, "mem"), field(sel, "upper"),
        ])
    elif "mem=" in sel:
        emit(disk_rows(sel))
    elif "cpus=" in sel:
        emit(mem_rows(sel))
    elif sel.startswith("image="):
        emit(cpu_rows(sel))


def main():
    phase = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("ROOST_PROVIDER_PHASE", "")
    cwd = os.environ.get("ROOST_ACTIVE_CWD", "")
    if phase == "list":
        do_list(cwd)
    elif phase == "activate":
        do_activate(cwd, os.environ.get("ROOST_SELECTED_ID", ""))


if __name__ == "__main__":
    main()
```

> **No free-text input (v1).** Every step is a *selection* — Roost has no
> "type a value" prompt. Derive what you can (here the name comes from the
> directory) or pre-list options; a typed name/port/path isn't expressible
> yet.

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
the same `{id, title, subtitle?}` item shape providers print — a provider
is just the Roost-driven version of the same contract. (One v1 difference:
`palette.present` rows are always selectable — the `actionable` flag is
honored for *provider* rows only, not over the `palette.present` wire.)
