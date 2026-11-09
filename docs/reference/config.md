# User config

Roost reads a single user-level config file at `~/.config/roost/config.conf`
(XDG-style on macOS by deliberate divergence from Apple HIG — matching
Ghostty / nvim / fish). The iced UI — `Roost.app` on macOS, `roost` on
Linux — parses the same file with the same semantics on both, so a config
tuned on one platform is portable to the other. A few
settings are honored by only one platform — where that is the case, the
setting's own section says so explicitly (see
[`link-modifier`](#link-modifier)); unknown and unsupported keys are
dropped rather than erroring, so a portable file stays valid everywhere.

The file is plain text, one `key = value` per line, `#`-prefixed comments
allowed, whitespace forgiving. Unknown keys are silently dropped — this
keeps user files forward-compatible with future Roost versions and with
keys that Ghostty consumes but Roost doesn't.

A missing file is fine: every setting has a compiled-in default.

To override the path for testing, set the `ROOST_CONFIG` environment
variable to an absolute file path. The E2E harness uses this to seed
the launcher with deterministic commands.

## Settings

| Key | Type | Default | Effect |
|---|---|---|---|
| `theme` | string | bundled `roost-dark` | Theme name (see [`themes.md`](themes.md)). |
| `font-family` | string | `JetBrains Mono, Monospace` (the UI bundles JetBrains Mono) | Monospaced font family. Quoted values supported (`"JetBrains Mono"`). See [Fonts](fonts.md). |
| `word-break-chars` | string | `` `_-.+~/:@%` `` | Extra characters treated as word characters for double-click word selection (keeps paths + URLs whole). Despite the `-break-` name (kept for Ghostty compatibility), the value is the extra word-char set. |
| `show-sidebar-agents` | bool | `true` | Whether the sidebar shows the Agents panel — every agent on every host in one list below the projects — and the agent count on each project row. Also toggled at runtime (keybind / palette / Mac View menu). |
| `agent-panel-sort` | `grouped \| priority \| recent` | `grouped` | How the sidebar's Agents panel orders its rows: in sidebar order under a head per host and project, most urgent first, or most recent first. Also cycled at runtime by the `cycle_agent_panel_sort` action and the palette's **Cycle Agent Panel Sort**, which write the choice back here. An unrecognized value logs a warning and reads as `grouped`. |
| `font-size` | number | `13` (Linux), `14` (Mac) | Point size for the terminal font. Must be `> 0`. One point is one pixel on macOS; Linux renders at 96 dpi (`13` ≈ 17.3 px). See [Fonts](fonts.md#font-size). |
| `keybind` | `<trigger> = <action>` | (see [Keybindings](../getting-started/keybindings.md)) | Append a custom keybinding. Repeatable; later entries override earlier ones. |
| `command` | `label="…" run="…" [hold=…]` | none | Launcher entry (⌘⇧T / Alt+Shift+T) that runs a fixed command in a new tab. Repeatable. See [Extending Roost](../guides/extending.md#2-the-command-launcher). |
| `provider` | `label="…" run="…" [timeout=…] [limit=…]` | none | Dynamic, script-backed menu in the custom palette (⌘⇧E / Alt+Shift+E). The script generates rows on demand and acts on the choice. Repeatable; executables in `providers/` (beside this file) are also discovered. See [Extending Roost](../guides/extending.md#3-dynamic-providers). |
| `copy-on-select` | `off \| true \| clipboard` | `true` | What a mouse-drag selection writes to the clipboard on release. See [the dedicated section below](#copy-on-select). |
| `clipboard-write` | `allow \| deny` | `allow` | Whether a program running in the terminal can write the host clipboard via OSC 52. See [the dedicated section below](#clipboard-write). |
| `link-modifier` | `ctrl \| alt \| super` | Cmd (Mac) / Alt (Linux) | Which held modifier reveals + opens a URL on hover/click. See [the dedicated section below](#link-modifier). |
| `macos-option-as-alt` | `false \| true \| left \| right` | `false` | Which Option keys send Alt (Meta) on macOS, so ⌥B sends `ESC b` instead of typing `∫`. Read on macOS only. See [the dedicated section below](#macos-option-as-alt). |
| `macos-auto-secure-input` | bool | `true` | Turn Secure Keyboard Entry on while the active tab is at a password prompt. Read on macOS only. See [Secure Keyboard Entry](#secure-keyboard-entry). |
| `macos-secure-input-indication` | bool | `true` | Show a lock at the right end of the tab band while Secure Keyboard Entry is on. Read on macOS only. See [Secure Keyboard Entry](#secure-keyboard-entry). |
| `macos-secure-keyboard-entry` | bool | `false` | Keep Secure Keyboard Entry on whenever Roost is the active app. Written by the menu's **Secure Keyboard Entry** row, the palette's **Toggle Secure Keyboard Entry** and the `toggle_secure_input` action. Read on macOS only. See [Secure Keyboard Entry](#secure-keyboard-entry). |
| `agent-integration` | departures from the defaults \| `off` \| absent | absent (the defaults: every agent but Cursor on) | Which coding agents Roost connects: Claude Code and OpenCode through the tab environment, Codex and Grok over OSC 7501, Cursor by writing its `hooks.json`. See [the dedicated section below](#agent-integration) and the [Agent integration](../guides/agents.md) guide. |
| `agent-hooks` | agent list \| `off` \| absent | absent | The older key, now a shadow Roost writes as the agents it connects by file (`cursor` or `off`), and reads only to carry an earlier answer over. See [below](#agent-hooks). |
| `local-backend` | `in-process \| session` | `in-process` for an existing setup; `session` on a genuinely fresh install | Where the tabs you start in the Roost window run. See [the dedicated section below](#local-backend). |
| `chrome-accent` | `system \| #rrggbb` | `system` | The accent color of the window chrome: the active tab and project, notification dots, focus rings. See [the dedicated section below](#chrome-accent). |

## `copy-on-select`

Controls what happens to a text selection the moment the user releases
the mouse after a drag. Vocabulary matches Ghostty 1:1 — three values
with platform-specific behaviors:

| Value | What it writes on drag-release | What pastes from there |
|---|---|---|
| `off` | nothing | n/a — user must press the explicit copy shortcut (⌘C on Mac, Ctrl+Shift+C on Linux) |
| `true` *(default)* | the **selection clipboard** only | middle-click anywhere in Roost; ⌘V / Ctrl+Shift+V are **not** affected |
| `clipboard` | both the selection clipboard **and** the system clipboard | middle-click (selection); ⌘V / Ctrl+Shift+V (system) |

### Per-platform details

**Linux.** The selection clipboard is the X11 / Wayland `PRIMARY`
selection — the conventional one-finger-middle-click target. The
system clipboard is `CLIPBOARD` (the Ctrl+C / Ctrl+V target most
non-terminal apps use). `true` matches the long-standing X11 convention
where dragging in a terminal writes PRIMARY but leaves CLIPBOARD
untouched.

**macOS.** Mac has no native equivalent of `PRIMARY`, so Roost
synthesizes one as a custom-named `NSPasteboard`
(`ai.stridelabs.Roost.selection`). Other Mac apps cannot read it — it
exists purely so Roost can offer middle-click paste without clobbering
the system pasteboard that ⌘V reads from. **Practical consequence:**
with the default `copy-on-select = true`, dragging a selection in
Roost and then pressing ⌘V in another app will paste whatever was last
⌘C'd, not the dragged text. This surprises Mac users who expect drag
to update the system clipboard. To get the "drag and paste anywhere"
behavior, set `copy-on-select = clipboard`.

### Middle-click paste

Middle-click anywhere in a Roost terminal pastes from the selection
clipboard, regardless of which `copy-on-select` value is set. The
gesture works on both platforms even when `copy-on-select = off`; in
that case the selection clipboard is just empty (whatever the user
last explicitly wrote via the copy shortcut isn't in there), so the
paste is a no-op.

Paste contents are routed through the same bracketed-paste-aware
encoder as ⌘V / Ctrl+Shift+V, so an `nvim` or `fish` that enables
DECSET 2004 receives `ESC[200~ … ESC[201~` wrappers around the
selection.

### Soft-wrapped lines

A line too long for the window is **soft-wrapped**: the terminal breaks
it across several screen rows purely to fit, with no newline in the
data. Copying rejoins those rows, so a wrapped line comes back as the
one long line it always was — the same thing Ghostty does. Paste it into
an editor and you get one line, not one per screen row, and a word split
across the break comes back whole.

Only the wraps the terminal made are absorbed. A newline the program
actually wrote still breaks the copy, and a wide (CJK / emoji) glyph
that had to move to the next row to fit copies once, in the right place.

### Trimming

The text written to either clipboard has trailing **spaces** stripped
per line (matches Ghostty's `clipboard-trim-trailing-spaces` default
behavior). Only `U+0020` is stripped — any other whitespace codepoint in
a cell is content the program deliberately wrote. "Line" means the
rejoined line, so spaces sitting at a soft-wrap boundary are interior
and survive; trimming them would glue two words together. Entirely-blank
trailing rows are dropped so a multi-row selection doesn't carry stray
newlines; leading and interior blank rows are kept, since they are part
of what was selected.

A selection that extends beyond the visible viewport copies **in full**,
including rows scrolled off-screen ([#249](https://github.com/charliek/roost/issues/249)).
Endpoints are tracked pins into the terminal's own storage
([#334](https://github.com/charliek/roost/issues/334)), so a selection
follows its **content**: it stays on the same characters as history
scrolls past, as the buffer drops rows off the top, and across a window
resize that reflows the line. A row that is **evicted** from scrollback
entirely takes the selection with it — copy then returns nothing, rather
than quietly handing back whichever row moved into that slot.

A selection also belongs to the screen it was made on. Start an
alt-screen program (vim, less, htop) and a selection made in the normal
scrollback stops being drawn and copies nothing; it reappears when the
program exits.

## `clipboard-write`

Controls whether a program running inside the terminal can write the
host clipboard by emitting the **OSC 52** escape sequence (`\e]52;c;<base64>\a`).
This is the path opencode-over-SSH, nvim with `g:clipboard = osc52`,
tmux `set -s set-clipboard on`, kitten ssh, yazi, and other TUIs use
to get text back to your local clipboard.

| Value | Behavior |
|---|---|
| `allow` *(default)* | OSC 52 writes the host clipboard. Matches Ghostty's default. |
| `deny` | OSC 52 sequences are parsed and silently dropped — logged at info, no clipboard side-effect. |

Phase 2 will add `ask` with a per-tab consent banner ("opencode wants
to write 42 bytes to your clipboard — Allow once / Always / Deny");
phase 1 is intentionally allow/deny only to keep the surface small.

### Read direction (OSC 52 `?`)

OSC 52 also supports a read direction (the program asks the terminal
to send the clipboard contents back). Roost **always drops** read
requests in phase 1 — there's no consent UI for them yet and reading
the clipboard from a remote process is the more sensitive direction
(shoulder-surfing a password manager value). This will become its own
`clipboard-read = allow | ask | deny` setting in phase 2.

This posture is unconditional across a host session's SSH transport too:
a read request is parsed and dropped identically whether the terminal
it arrived on is local or attached over SSH to a remote host — reading
is *more* sensitive, not less, when the process asking runs on a
machine you don't sit at, so there's no separate carve-out for it. A
**write** from a program on a remote host still lands on **this**
machine's clipboard, subject to the same `clipboard-write` setting
above — the session forwards the OSC 52 write to whichever client
currently holds the tab, and that client's own `clipboard-write`
config decides whether it's applied.

### Targets

OSC 52 carries a `Ps` selector indicating which clipboard to write:

- `c` (default) → system clipboard (`NSPasteboard.general` on Mac,
  `CLIPBOARD` on Linux — what ⌘V / Ctrl+V pastes from).
- `p` or `s` → selection clipboard (named `NSPasteboard` on Mac,
  X11 / Wayland `PRIMARY` on Linux — what middle-click pastes from).
- Any other selector falls through to system (matches Ghostty's
  permissive handling of emitters that pad the selector with letters).

## `link-modifier`

Holding this modifier while hovering a URL reveals it (underline + hand
cursor); holding it while clicking opens it in your default browser.
URLs come from both OSC 8 hyperlinks (what tools like Claude Code emit)
and plain `https://…` text matched on screen.

| Value | Modifier |
|---|---|
| `ctrl` | Control |
| `alt` | Alt / Option |
| `super` *(aliases: `cmd`, `command`, `meta`)* | Super / Command |

**Defaults are platform-native**, mirroring the keybinding scheme:

- **macOS: Cmd** — matches native Mac apps (⌘-click).
- **Linux: Alt** — Roost's single "primary" modifier on Linux, leaving
  Ctrl free for the shell/readline.

**Linux users who prefer the traditional Ctrl+click** (the ghostty /
common-terminal convention) just set:

```conf
link-modifier = ctrl
```

> **Scope:** this setting is honored on both platforms
> (`link_modifier_held` in `crates/roost-iced/src/app/interactions.rs`).
>
> **Heads up (Linux):** some window managers/compositors grab `Alt`+drag
> to move windows, which can swallow `Alt`+click. If link-clicking feels
> flaky on your desktop, switch to `link-modifier = ctrl`.

### Prefer Ctrl across the board? (Linux)

On Linux Roost defaults to an **Alt-centric** scheme (`Alt+T` new tab,
`Alt+W` close, etc.) so Ctrl stays free for the shell — the only Ctrl
defaults are `Ctrl+1`…`Ctrl+9` (tab switching) and `Ctrl+Shift+C/V`. If you'd rather
use the more familiar Ctrl shortcuts, you can remap both the link
modifier and any keybinding — `keybind` lines are repeatable and
last-wins:

```conf
# Ctrl+click to open links
link-modifier = ctrl

# Restore Ctrl-based shortcuts (examples — see the keybindings doc)
keybind = ctrl+t = new_tab
keybind = ctrl+w = close_tab
```

See [Keybindings](../getting-started/keybindings.md) for the full action
list and trigger syntax.

## `macos-option-as-alt`

On macOS, Option is a text modifier: ⌥B types `∫` and ⌥F types `ƒ`.
Shells and editors that use Meta chords, such as readline's ⌥B and ⌥F
word jumps, want those keys to send `ESC b` and `ESC f` instead. This
setting makes Option act as Alt. The key and its values are Ghostty's.

| Value | Which Option sends Alt |
|---|---|
| `false` (default) | neither; both type macOS characters |
| `true` | both |
| `left` | the left Option; the right one still types characters |
| `right` | the right Option; the left one still types characters |

Values are case-insensitive. An unrecognized value logs a warning and
reads as `false`.

```conf
macos-option-as-alt = left
```

When Option acts as Alt, it sends the key without Option's character:
⌥B sends `ESC b`, and ⇧⌥B sends `ESC B`. Two kinds of key still type
the character macOS gives them:

- a shifted key that is not a letter, such as ⇧⌥1, because its shifted
  character depends on the keyboard layout;
- a key whose own character is not ASCII, such as `é` on a French
  layout, because the Meta prefix carries a single byte.

**Limitations:**

- The dead-key chords ⌥E, ⌥U, ⌥N, ⌥I and ⌥\` are taken by macOS's input
  method before Roost sees them, so they still start an accent
  ([#582](https://github.com/charliek/roost/issues/582)).
- `left` and `right` tell the two Options apart by the Option key's own
  press and release. If Roost has seen neither while Option is held, it
  counts that Option as the configured side, and logs this once.

> **Scope:** read on macOS. On Linux, Alt already sends the ESC prefix,
> so the key is parsed and ignored.

## Secure Keyboard Entry

While Secure Keyboard Entry is on, macOS stops other processes — event
taps, keyloggers, some accessibility tools — from reading what you
type. Terminal.app, iTerm2 and Ghostty all offer it. Roost follows
Ghostty's model, and uses Ghostty's key names:

| Key | Default | Meaning |
|---|---|---|
| `macos-auto-secure-input` | `true` | Turn it on while the active tab is at a password prompt. |
| `macos-secure-input-indication` | `true` | Show the lock while it is on. |
| `macos-secure-keyboard-entry` | `false` | Keep it on whenever Roost is the active app. |

```conf
macos-auto-secure-input = true
macos-secure-keyboard-entry = false
```

Secure Keyboard Entry is on while Roost is the active app and either
`macos-secure-keyboard-entry` is on, or `macos-auto-secure-input` is on
and the active tab is at a password prompt. Switching to another app
turns it off at once, including while one of Roost's menus is open, and
coming back turns it back on. Quitting Roost always gives it back.

**The toggle.** The app menu's **Secure Keyboard Entry** row, just
below **Settings…**, shows a checkmark while `macos-secure-keyboard-entry`
is on and flips it. The command palette's **Toggle Secure Keyboard
Entry** row and the `toggle_secure_input` action do the same; the
action has no default shortcut (see
[Keybindings](../getting-started/keybindings.md#available-actions)).
The choice is written back to `config.conf`, so it survives a restart.

**The lock.** While it is on, a small lock sits at the right end of the
tab band; hover it for an explanation. `macos-secure-input-indication =
false` hides it.

**How a password prompt is recognised.** A tab is at a password prompt
while its terminal is in line mode with echo off — what `sudo`, `ssh`'s
own password prompt, `passwd` and `read -s` set. Roost checks every
200 ms. Only the active tab counts: a prompt in a background tab turns
nothing on until you switch to it. Any program that turns echo off in
line mode is treated as a prompt; set `macos-auto-secure-input = false`
if that gets in the way, for example of accessibility software.

**Remote prompts.** A host-session tab, whose shell runs in a
`roost-session` on another machine, does catch a remote `sudo`: the
session watches its own terminals and reports a prompt to Roost. That
needs a `roost-session` new enough to report it; an older one never
does, and its tabs never turn Secure Keyboard Entry on by themselves. A
plain `ssh` run in a local tab is different: `ssh` puts the local
terminal in raw mode, so a password prompt on the far side is never
seen. Turn on `macos-secure-keyboard-entry` for those.

> **Scope:** macOS. On Linux the three keys are parsed and ignored,
> `toggle_secure_input` does nothing, and there is no menu row, palette
> row or lock.

## `agent-integration`

Which coding agents Roost connects, whatever the way: a plugin in the
tab environment (Claude Code, OpenCode), the agent's own OSC 7501
reports (Codex, Grok), or entries Roost writes into the agent's file
(Cursor). See [Agent integration](../guides/agents.md) for what each
means. Read at every tab spawn, so a change reaches the next tab.

```conf
agent-integration = -opencode   # departures from the defaults, or off
```

| Value | Effect |
|---|---|
| absent or empty | **The defaults**: every agent on except Cursor. A fresh install starts here, and no card is shown unless Cursor is installed (its opt-in is still unanswered). |
| `name` | Turns that agent on (`cursor` is the only one that is off by default). |
| `-name` | Turns that agent off. |
| `off` (also `false`, `no`, `none`) | Every agent off, as the whole value or as a token: `off, cursor` is only Cursor. |

**Departures, not a list.** An agent the value doesn't name keeps its
default, so an agent a later release adds is not switched off by a value
written before it existed. Tokens are trimmed and lowercased, the last
token for a name wins, and a name this build doesn't know is kept and
changes nothing. The Agent Integration card's Apply and `roostctl agent
set` write a full value (every default-on agent named, on or off).

**What off means.** Claude Code or OpenCode off: the next tab gets no
plugin variable. Cursor off: its file entries are removed. Codex or Grok
off: recorded, but no effect yet, because both report over OSC 7501 on
their own.

**An unreadable `config.conf`** gives new tabs no agent plugins, since
the file might say `off`.

## `agent-hooks`

The older key, from releases that wrote hook entries into every agent's
own files. It is now a **shadow**: Roost writes it as the agents it
still connects by file (`cursor`, or `off`) and reads it only to carry
an earlier answer over:

- With no `agent-integration`, an answered `agent-hooks` decides:
  `off` is every agent off and a list is those agents on, the rest off.
  The first launch of this release writes the same answer into
  `agent-integration` and narrows `agent-hooks` to the shadow. See
  [Upgrading: the retire](../guides/agents.md#upgrading-the-retire).
- An older Roost on the same machine reads only `agent-hooks`. Narrowed
  to the file agents, it no longer tells that Roost to wire Claude's
  file, so the two builds don't take turns adding and removing entries.

```conf
agent-hooks = cursor   # written by Roost; a list, off, or absent
```

The parsing rules are unchanged: names trim, lowercase, de-duplicate and
reorder into `claude, codex, grok, cursor, opencode`; an unknown name is
kept and warned about; `off`/`false`/`no` is off; the retired
`auto`/`on`/`true`/`yes`, or a value naming no known agent, reads as
unanswered with a warning.

**Over a connected host session** a client still raises the host's
`agent-hooks` key, never lowering it, and a host whose key is `off` or
unanswered is raised anyway: a host has no screen to ask on. Since the
key now names only Cursor, that raise is Cursor's file opt-in; a host's
Claude Code and OpenCode follow the host's own `agent-integration`.
Lowering a host is done on that machine (`roostctl agent set` or
`uninstall` run there). See [Agent integration → Remote
hosts](../guides/agents.md#remote-hosts).

## `local-backend`

Where the tabs you start in the Roost window run: in the UI process
(`in-process`, they end when Roost quits), or in a local [host
session](../guides/host-sessions.md) (`session`, they keep running, and
any of your own clients can attach). See the [Switching the local
backend](../guides/host-sessions.md#switching-the-local-backend) guide
section for the two palette rows that move between them live, with no
config editing or relaunch required.

```conf
local-backend = in-process   # in-process (existing setups) | session (fresh installs)
```

**The default depends on what's already on disk, not on the platform.**
A genuinely **fresh install** — no `state.json` and no `config.conf`
anywhere Roost would look — starts on `session` from its very first
launch, and Roost writes the key to `config.conf` for you so the choice
is visible and stays put. Any machine that already has either file
keeps `in-process`, exactly as it always has, until you switch it
yourself. This asymmetry is deliberate: only a machine with nothing of
this app on it yet is a machine where "session" isn't a behavior change
to a saved layout.

An unrecognized value (a typo, an older config surviving a downgrade)
logs a warning and reads as `in-process` — configured badly, not
unconfigured, so it never triggers the fresh-install write on a
later launch.

## `chrome-accent`

The accent color of the window chrome around the terminal: the active
tab pill and project row, the notification dots, the outline of a
dragged tab, the focus ring, the confirm button and the command
palette's match highlight.

| Value | Accent |
|---|---|
| `system` *(default)* | **macOS:** the system accent color (System Settings → Appearance), followed live when you change it. **Linux:** Roost's blue, `#007aff` — the desktop's accent is not followed. |
| `#rrggbb` | That color, on both platforms. Hex digits in either case. |

Under Roost's blue the chrome keeps its hand-picked fills. Any other
accent derives them the way macOS's own controls do: the active tab is
the accent at 18% over the tab band, and the selected project row is the
accent mixed half and half with a dark grey.

Anything else (a color name, a short hex) logs a warning and reads as
`system`.

## Example

```
# ~/.config/roost/config.conf

theme = Catppuccin Mocha
font-family = "JetBrains Mono"
font-size = 14

# Drag-to-select writes to PRIMARY (Linux) / Roost's named selection
# pasteboard (Mac). Middle-click pastes. Cmd-V / Ctrl+Shift+V are
# untouched.
copy-on-select = true

# To make drag-to-select also write the system clipboard, so Cmd-V
# in another Mac app gets the dragged text:
#
#   copy-on-select = clipboard

# Default: programs in the terminal can write your clipboard via
# OSC 52 (the opencode-over-SSH path, nvim's g:clipboard = osc52,
# tmux set-clipboard, etc.). Set `deny` to opt out.
clipboard-write = allow

# Open links with Ctrl+click instead of the platform default
# (Cmd on Mac, Alt on Linux).
link-modifier = ctrl

# Add a custom trigger (here, restoring the pre-Alt Ctrl+T for new_tab
# on Linux — the default is now Alt+T). See docs/getting-started/keybindings.md.
keybind = ctrl+t = new_tab

# A fixed launcher command (⌘⇧T / Alt+Shift+T):
command = label="Claude" run="claude --resume"

# A dynamic, script-backed menu (⌘⇧E / Alt+Shift+E). The script prints
# its rows on `list` and acts on the choice on `activate`:
provider = label="Open shed" run="~/.config/roost/providers/shed.sh"

# Left absent (the default), Roost connects every agent but Cursor
# without writing to their settings files. Record departures only:
# here, OpenCode off and Cursor's hooks.json opt-in on.
agent-integration = -opencode, cursor
```

See [Extending Roost](../guides/extending.md) for the full `command =` /
`provider =` contract (with bash / Python / TypeScript examples),
[`paths.md`](paths.md) for where the file lives on each platform,
[`themes.md`](themes.md) for the `theme` value enumeration, and the
[Agent integration](../guides/agents.md) guide for `agent-integration` in full.
