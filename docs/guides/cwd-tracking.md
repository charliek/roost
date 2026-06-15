# Tracking the working directory

Three places in Roost care what directory a tab is "in":

- the cwd a new tab inherits on `Alt-T` / `Cmd-T` (and the command launcher),
- the header subtitle under the project name,
- the tab label (until you rename the tab or the running program sets its own title).

## What works out of the box

**New tabs open where you are.** On `Alt-T` / `Cmd-T`, Roost reads the active
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
startup files (aliases, prompt, `PROMPT_COMMAND` hooks) load exactly as they do
outside Roost.

**Login vs. non-login shell** (this decides *which* startup files run). Roost
follows the same platform split as Ghostty: a default tab is a **login shell on
macOS** (sources `/etc/profile` then the first of `.bash_profile` / `.bash_login`
/ `.profile`; `.zprofile` / `.zlogin` for zsh) and a **non-login interactive
shell on Linux** (sources `/etc/bash.bashrc` then `~/.bashrc`; `~/.zshrc` for
zsh). macOS GUI apps don't inherit the login `PATH` and the macOS world keeps
config in `.bash_profile`, so a login shell is expected there; on Linux the
desktop session already exports the login `PATH`, and config conventionally
lives in `~/.bashrc`, which only a non-login shell sources. If you keep bash
config in `~/.bashrc` but also have a `~/.bash_profile` (some tool installers
drop one in), note that on macOS the login shell stops at `.bash_profile` and
never reads `.bashrc` unless `.bash_profile` sources it — the standard fix is to
add `[ -f ~/.bashrc ] && . ~/.bashrc` to `~/.bash_profile`.

Two cases auto-loading can't reach, where you add one line to your rc instead:

- **Apple's `/bin/bash` (3.2)** on macOS — its `ENV`/POSIX startup path is
  patched out, so Roost can't inject. Homebrew bash (or any bash ≥ 4.4) auto-loads,
  but only when it's your **login shell**: Roost reads `$SHELL`, not `$PATH`. See
  [Switching macOS default to Homebrew bash](#switching-macos-default-to-homebrew-bash)
  below if `which bash` shows Homebrew but default tabs still spawn Apple's.
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

### Switching macOS default to Homebrew bash

A common confusion: you've run `brew install bash`, and `which bash` returns
`/opt/homebrew/bin/bash` (5.x) — but a default Cmd-T tab still spawns Apple's
3.2 and falls back to the manual-source path above. The two commands answer
different questions:

- `which bash` walks **`$PATH`** and reports the first executable named `bash`
  — Homebrew puts `/opt/homebrew/bin` ahead of `/bin` via `brew shellenv`, so
  this finds the modern one. It's "if I type `bash`, what runs?".
- `$SHELL` is your **registered login shell** — the account property that
  `chsh` sets, stored in macOS's directory service (`dscl . -read /Users/$USER
  UserShell`). It's "what shell does this user prefer?", which is what Roost,
  Terminal.app, `cron`, `sudo -s`, IDE terminals, and everything else asks when
  they need to *spawn* a shell. It doesn't follow `$PATH`.

Until you `chsh`, `$SHELL` stays at the registered value (Apple `/bin/bash` on
many older or migrated macOS accounts), so default tabs use Apple bash even
though `which bash` shows Homebrew. To make modern bash your login shell so
every default tab auto-bootstraps:

```bash
# 1. Allow it as a login shell (macOS keeps an allow-list).
grep -qx /opt/homebrew/bin/bash /etc/shells \
  || echo /opt/homebrew/bin/bash | sudo tee -a /etc/shells

# 2. Switch (prompts for your account password).
chsh -s /opt/homebrew/bin/bash
```

Then **fully quit and relaunch Roost** so the GUI process inherits the new
`$SHELL` from its parent environment. Verify in a fresh tab:

```bash
echo "$SHELL"; "$SHELL" --version | head -1
# expect: /opt/homebrew/bin/bash + 5.x
```

If you'd rather not switch your account default, the manual-source line above
works on Apple `/bin/bash`, and you can always open a single Homebrew-bash tab
on demand via `roostctl tab open --argv /opt/homebrew/bin/bash --argv -l`
(handy as a saved command-launcher entry).

The scripts are gated on `$ROOST_TAB_ID`, idempotent, and interactive-only. They
emit OSC 7 (cwd) and OSC 0 (a `~`-abbreviated path as the tab title), and set a
default prompt only when `PS1` is unset or the shell's stock default.

### Feature flags

`$ROOST_SHELL_FEATURES` is a comma list; prefix a feature with `no-` to disable
it. Default: `cwd,title,marks,prompt,ssh-env`.

- `cwd` — emit OSC 7 (the working directory).
- `title` — set the tab title to the cwd.
- `marks` — emit OSC 133 command marks (these drive the tab's run-state dot).
- `prompt` — set a default prompt (only when you haven't set one).
- `ssh-env` — wrap `ssh` so it adds
  `-o "SendEnv COLORTERM TERM_PROGRAM TERM_PROGRAM_VERSION FORCE_HYPERLINK"`
  to every invocation. Without this, macOS's default `ssh_config` only forwards
  `LANG LC_*` — `COLORTERM` is silently dropped at the SSH boundary
  and modern TUIs (opencode, neovim with truecolor themes) fall back
  to 256-color and look washed out on the remote host. Equivalent to
  Ghostty's `shell-integration-features.ssh-env`. Whether the remote
  *accepts* the forwarded vars depends on its `sshd_config::AcceptEnv`
  setting; if the server rejects them, SendEnv is a silent no-op (no
  worse than current behavior). Scoped to bare `ssh` — `scp`, `rsync`,
  and `git push` use their own binaries and aren't wrapped.

The flags are opt-*out*: every feature is on unless its `no-` form is present.
So to keep your own title and prompt, set
`ROOST_SHELL_FEATURES=no-title,no-prompt` in your rc — auto-loading re-sources
your rc first, so the override is picked up before Roost's hooks apply. The
same opt-out works for `ssh-env`: `ROOST_SHELL_FEATURES=...,no-ssh-env` to
disable.

## The environment Roost injects

Every shell Roost spawns sees:

| Variable                  | Meaning                                                                       |
|---------------------------|-------------------------------------------------------------------------------|
| `ROOST_TAB_ID`            | the tab's id — gate your integration on this                                  |
| `ROOST_SOCKET`            | the IPC socket path (`roostctl` auto-detects it)                              |
| `ROOST_RESOURCES_DIR`     | where the shipped scripts live (`…/shell-integration/`)                       |
| `ROOST_SHELL_INTEGRATION` | `1`                                                                           |
| `ROOST_SHELL_FEATURES`    | feature flags (above)                                                         |
| `TERM_PROGRAM`            | `Roost` (plus `TERM_PROGRAM_VERSION`)                                         |
| `TERM`                    | `xterm-256color`                                                              |
| `COLORTERM`               | `truecolor` — signals 24-bit color to TUIs (forwarded over SSH via `ssh-env`) |
| `FORCE_HYPERLINK`         | `1` — advertises OSC 8 hyperlink support so `supports-hyperlinks`-gated CLIs (Claude Code et al.) emit clickable links (forwarded over SSH via `ssh-env`) |

You don't have to set `ROOST_SOCKET`, `ROOST_TAB_ID`, or `ROOST_RESOURCES_DIR` —
Roost injects them. The full authoritative table is in
[`docs/reference/paths.md`](../reference/paths.md#environment-variables-roost-sets),
which also covers the internal-bootstrap vars (`ZDOTDIR`, `ENV`,
`ROOST_BASH_*`) that you shouldn't depend on from user code.

## Fancier: a git-aware title with a 🐓

Want the tab label to show a status icon + branch instead of just the path? Set
`ROOST_SHELL_FEATURES=no-title` (so the shipped title stays out of its way) and
add this to your rc — its `__roost_fancy_title` becomes the only thing setting
the title. 🐓 = clean tree or outside a repo, 🐣 = dirty tree.

```bash
__roost_fancy_title() {
  [ -n "$ROOST_TAB_ID" ] || return
  local icon="🐓" branch title
  case "$PWD" in
    "$HOME")    title="~" ;;
    "$HOME"/*)  title="~${PWD#"$HOME"}" ;;
    *)          title="$PWD" ;;
  esac
  if branch=$(git symbolic-ref --short HEAD 2>/dev/null); then
    [ -n "$(git status --porcelain 2>/dev/null)" ] && icon="🐣"
    title+=" (${branch})"
  fi
  printf '\033]0;%s\033\\' "${icon} ${title}"
}
PROMPT_COMMAND="__roost_fancy_title;${PROMPT_COMMAND}"
```

The `$HOME → ~` abbreviation uses a `case` + prefix-strip rather than the
tempting `${PWD/#$HOME/~}`, because that one-liner **isn't portable**: macOS's
`/bin/bash` (3.2) treats the replacement `~` as a literal, but bash ≥ 5.2
tilde-expands it back to `$HOME` — a silent no-op that shows the full
`/home/you/...` path. Escaping it (`\~`) flips the breakage to the other
version. The `case` form is correct on both and leaves non-home paths untouched.

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
