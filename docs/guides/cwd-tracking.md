# Tracking the working directory

Three places in Roost want to know what directory each tab is "in":

- The header subtitle under the project name.
- The tab label (when the tab hasn't been renamed and the running program hasn't set its own title).
- The cwd a new tab inherits when you press `Ctrl-T` / `Cmd-T`.

All three read from the same source: **OSC 7 escape sequences emitted by the shell on every prompt**. If your shell emits them, the subtitle and tab label follow `cd` and new tabs open where the previous one left off. If it doesn't, you'll see a static path (the project's initial cwd) everywhere.

Fish emits OSC 7 by default. Bash and zsh do not — you have to add a one-liner.

## Is it already working?

From inside a Roost tab, run:

```bash
printf '\e]7;file:///tmp\e\\'
```

The header subtitle should immediately flip to `/tmp`. If it does, OSC 7 reception works — the only question is whether your shell is auto-emitting it. Test that with a real `cd`:

```bash
cd /tmp
```

If the subtitle updates, you're done. If it stays put, your shell isn't emitting OSC 7 and you want one of the snippets below.

## Snippets

Pick one. They get fancier as you go.

### Minimal

**bash** (in `~/.bashrc`):

```bash
PROMPT_COMMAND='printf "\e]7;file://%s%s\e\\" "$HOSTNAME" "$PWD";'"$PROMPT_COMMAND"
```

**zsh** (in `~/.zshrc`):

```zsh
_osc7() { printf '\e]7;file://%s%s\e\\' "$HOST" "$PWD" }
chpwd_functions+=(_osc7)
_osc7  # initial emit, since chpwd doesn't fire for the starting cwd
```

These fire from every shell session you open, in any terminal. Harmless — terminals that don't understand OSC 7 ignore it — but if you want to keep your dotfiles tidy, see the gated version next.

### Gated on `$ROOST_TAB_ID`

Roost sets `ROOST_TAB_ID` on every shell it spawns (alongside `ROOST_PROJECT_ID` and `ROOST_SOCKET`). Gating on it means the snippet only fires inside Roost.

**bash:**

```bash
__roost_osc7() {
  [ -n "$ROOST_TAB_ID" ] || return
  printf '\e]7;file://%s%s\e\\' "$HOSTNAME" "$PWD"
}
PROMPT_COMMAND='__roost_osc7;'"$PROMPT_COMMAND"
```

**zsh:**

```zsh
_roost_osc7() {
  [ -n "$ROOST_TAB_ID" ] || return
  printf '\e]7;file://%s%s\e\\' "$HOST" "$PWD"
}
chpwd_functions+=(_roost_osc7)
_roost_osc7
```

### Full-featured: also set the tab title with a git indicator

This emits OSC 7 *and* OSC 0. OSC 0 sets the tab title, so the tab label becomes a tilde-abbreviated path with a status icon (🐓 outside a repo or clean tree, 🐣 dirty tree) and the current branch in parentheses. The header subtitle still tracks the raw cwd via OSC 7.

```bash
__roost_osc7() {
  [ -n "$ROOST_TAB_ID" ] || return
  printf '\e]7;file://%s%s\e\\' "$HOSTNAME" "$PWD"
}
__roost_title() {
  [ -n "$ROOST_TAB_ID" ] || return
  local icon="🐓"
  local title="${PWD/#$HOME/~}"
  local branch
  if branch=$(git symbolic-ref --short HEAD 2>/dev/null); then
    if [ -n "$(git status --porcelain 2>/dev/null)" ]; then
      icon="🐣"
    else
      icon="🐓"
    fi
    title+=" (${branch})"
  fi
  printf '\e]0;%s\e\\' "${icon} ${title}"
}
PROMPT_COMMAND="__roost_osc7;__roost_title;${PROMPT_COMMAND}"
```

The `git status --porcelain` call runs once per prompt; if you have very large repos and a slow disk it's worth knowing about.

## What lights up once OSC 7 is flowing

| Surface                           | Behavior                                                                             |
|-----------------------------------|--------------------------------------------------------------------------------------|
| Header subtitle                   | Follows `cd`, abbreviates `$HOME` to `~`, truncates from the left if too long        |
| Tab label                         | Same, *unless* you've renamed the tab or the program inside is emitting its own OSC 0/1/2 title (which always wins) |
| New-tab cwd                       | `Ctrl-T` / `Cmd-T` opens in the active tab's live cwd, not its starting cwd          |

If the running program in a tab sets its own title (vim, ssh, claude, the OSC 0 snippet above), the tab label shows that title and the header subtitle still shows the raw cwd. If you manually rename a tab via the `rename_tab` keybinding, that name sticks regardless.

## What you don't have to do

- You don't have to set `ROOST_SOCKET` or `ROOST_TAB_ID`. Roost injects them itself.
- You don't have to install anything. OSC 7 is plain shell output.
- You don't have to do anything in fish — it already emits OSC 7 in `fish_prompt`.
