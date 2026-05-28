# Tracking the working directory

Three places in Roost care what directory a tab is "in":

- the cwd a new tab inherits on `Ctrl-T` / `Cmd-T` (and the command launcher),
- the header subtitle under the project name,
- the tab label (until you rename the tab or the running program sets its own title).

## What works out of the box

**New tabs open where you are.** On `Ctrl-T` / `Cmd-T`, Roost reads the active
tab's shell working directory directly — natively, via `proc_pidinfo` on macOS
and `/proc/<pid>/cwd` on Linux — and spawns the new tab there. No shell
configuration required; works for any shell, including stock `/bin/bash`.

One caveat: a new tab spawns a *local* shell, so if the active tab is `ssh`'d to
a remote host, the new tab opens in the local directory, not the remote one. To
track a remote cwd, use the shell integration below — the remote shell emits
OSC 7 over the connection.

## What the shell integration adds

Sourcing Roost's integration makes the shell emit **OSC 7** on every prompt,
which adds:

- the **header subtitle** following `cd` live,
- the **tab label** following `cd` live,
- **remote (SSH) cwd** tracking,

plus (optionally) a tidy default **prompt** when you don't already have one.

Fish emits OSC 7 natively, so it needs nothing.

## How it loads

For **zsh** and **modern bash (≥ 4.4)** Roost loads the integration
automatically — **no rc edit**. It points the shell at the shipped script
(`ZDOTDIR` for zsh; `--posix` + `ENV` for bash), runs your normal startup files
first, then layers the integration on top, so your config still wins. Your real
login + rc files (`.bash_profile`/`.bashrc`, `.zprofile`/`.zshrc`/`.zlogin`,
aliases, prompt, `PROMPT_COMMAND` hooks) load exactly as they do outside Roost.

Two cases auto-loading can't reach, where you add one line to your rc instead:

- **Apple's `/bin/bash` (3.2)** on macOS — its `ENV`/POSIX startup path is
  patched out, so Roost can't inject. (Homebrew bash, or any bash ≥ 4.4, works
  automatically.)
- **zsh with a system `/etc/zshenv` that hard-sets `ZDOTDIR`** — it runs before
  Roost's shim and overrides it.

The manual line, safe in a shared dotfile (the `$ROOST_TAB_ID` guard makes it a
no-op outside Roost, and the script is idempotent if auto-loading already ran):

**bash** (`~/.bashrc`):

```bash
[ -n "$ROOST_TAB_ID" ] && [ -r "$ROOST_RESOURCES_DIR/shell-integration/roost.bash" ] \
  && source "$ROOST_RESOURCES_DIR/shell-integration/roost.bash"
```

**zsh** (`~/.zshrc`):

```zsh
[[ -n "$ROOST_TAB_ID" && -r "$ROOST_RESOURCES_DIR/shell-integration/roost.zsh" ]] \
  && source "$ROOST_RESOURCES_DIR/shell-integration/roost.zsh"
```

Roost ships the scripts inside the app and points `$ROOST_RESOURCES_DIR` at them.

The scripts are gated on `$ROOST_TAB_ID`, idempotent, and interactive-only. They
emit OSC 7 (cwd) and OSC 0 (a `~`-abbreviated path as the tab title), and set a
default prompt only when `PS1` is unset or the shell's stock default.

### Feature flags

`$ROOST_SHELL_FEATURES` is a comma list; prefix a feature with `no-` to disable
it. Default: `cwd,title,prompt`.

- `cwd` — emit OSC 7.
- `title` — set the tab title to the cwd.
- `prompt` — set a default prompt (only when you haven't set one).

The flags are opt-*out*: every feature is on unless its `no-` form is present.
So to keep your own title and prompt, set
`ROOST_SHELL_FEATURES=no-title,no-prompt` in your rc before sourcing.

## The environment Roost injects

Every shell Roost spawns sees:

| Variable                  | Meaning                                                 |
|---------------------------|---------------------------------------------------------|
| `ROOST_TAB_ID`            | the tab's id — gate your integration on this            |
| `ROOST_SOCKET`            | the IPC socket path (`roostctl` auto-detects it)        |
| `ROOST_RESOURCES_DIR`     | where the shipped scripts live (`…/shell-integration/`) |
| `ROOST_SHELL_INTEGRATION` | `1`                                                     |
| `ROOST_SHELL_FEATURES`    | feature flags (above)                                   |
| `TERM_PROGRAM`            | `Roost` (plus `TERM_PROGRAM_VERSION`)                   |
| `TERM`                    | `xterm-256color`                                        |

You don't have to set `ROOST_SOCKET`, `ROOST_TAB_ID`, or `ROOST_RESOURCES_DIR` —
Roost injects them.

## Fancier: a git-aware title with a 🐓

Want the tab label to show a status icon + branch instead of just the path? Set
`ROOST_SHELL_FEATURES=no-title` (so the shipped title stays out of its way) and
add this to your rc — its `__roost_fancy_title` becomes the only thing setting
the title. 🐓 = clean tree or outside a repo, 🐣 = dirty tree.

```bash
__roost_fancy_title() {
  [ -n "$ROOST_TAB_ID" ] || return
  local icon="🐓" title="${PWD/#$HOME/~}" branch
  if branch=$(git symbolic-ref --short HEAD 2>/dev/null); then
    [ -n "$(git status --porcelain 2>/dev/null)" ] && icon="🐣"
    title+=" (${branch})"
  fi
  printf '\033]0;%s\033\\' "${icon} ${title}"
}
PROMPT_COMMAND="__roost_fancy_title;${PROMPT_COMMAND}"
```

The `git status --porcelain` runs once per prompt; on very large repos with a
slow disk that's worth knowing about.

## What lights up

| Surface         | Behavior                                                                              |
|-----------------|---------------------------------------------------------------------------------------|
| New-tab cwd     | Always follows the active tab's current dir (native read) — integration or not.       |
| Header subtitle | Follows `cd` once OSC 7 is flowing (shell integration).                                |
| Tab label       | Same — unless you renamed the tab or the running program set its own title (it wins).  |

If the program in a tab sets its own title (vim, ssh, claude), that shows and the
subtitle still tracks the raw cwd. A manual rename sticks regardless.
