# User config

Roost reads a single user-level config file at `~/.config/roost/config.conf`
(XDG-style on macOS by deliberate divergence from Apple HIG — matching
Ghostty / nvim / fish). Both UIs — the Swift Mac app and the iced binary
Linux installs as `roost` — parse the same file with the same semantics,
so a config tuned on one platform is portable to the other. A few
settings are honored by only one of them — where that is the case, the
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
| `font-family` | string | system monospace (Mac) / `JetBrains Mono, Monospace` (Linux) | Monospaced font family. Quoted values supported (`"JetBrains Mono"`). See [Fonts](fonts.md). |
| `word-break-chars` | string | `` `_-.+~/:@%` `` | Extra characters treated as word characters for double-click word selection (keeps paths + URLs whole). Despite the `-break-` name (kept for Ghostty compatibility), the value is the extra word-char set. |
| `show-sidebar-agents` | bool | `true` | Whether the sidebar renders one row per agent-owned tab under its project. Also toggled at runtime (keybind / palette / Mac View menu). |
| `font-size` | number | `13` (Linux), `14` (Mac) | Point size for the terminal font. Must be `> 0`. |
| `tab-min-width` | number | `80` (Mac) | Minimum tab pill width in points. `0` disables the floor. Mac-only. |
| `tab-max-width` | number | `220` (Mac) | Maximum tab pill width in points. `0` disables the cap (pills grow to fit). Mac-only. |
| `keybind` | `<trigger> = <action>` | (see [Keybindings](../getting-started/keybindings.md)) | Append a custom keybinding. Repeatable; later entries override earlier ones. |
| `command` | `label="…" run="…" [hold=…]` | none | Launcher entry (⌘⇧T / Alt+Shift+T) that runs a fixed command in a new tab. Repeatable. See [Extending Roost](../guides/extending.md#2-the-command-launcher). |
| `provider` | `label="…" run="…" [timeout=…] [limit=…]` | none | Dynamic, script-backed menu in the custom palette (⌘⇧E / Alt+Shift+E). The script generates rows on demand and acts on the choice. Repeatable; executables in `providers/` (beside this file) are also discovered. See [Extending Roost](../guides/extending.md#3-dynamic-providers). |
| `copy-on-select` | `off \| true \| clipboard` | `true` | What a mouse-drag selection writes to the clipboard on release. See [the dedicated section below](#copy-on-select). |
| `clipboard-write` | `allow \| deny` | `allow` | Whether a program running in the terminal can write the host clipboard via OSC 52. See [the dedicated section below](#clipboard-write). |
| `link-modifier` | `ctrl \| alt \| super` | Cmd (Mac) / Alt (Linux) | Which held modifier reveals + opens a URL on hover/click. iced-only; the Swift Mac app is fixed to Cmd. See [the dedicated section below](#link-modifier). |
| `agent-hooks` | agent list \| `off` \| absent | absent (unanswered) | Which supported coding agents (Claude Code, Codex, grok/gx, cursor-agent, OpenCode) Roost wires its hook entries into, at startup. Absent means nobody has answered the consent dialog yet — Roost writes nothing until they do. See [the dedicated section below](#agent-hooks) and the [Agent Hooks](../guides/agents.md) guide. |
| `local-backend` | `in-process \| session` | `in-process` for an existing setup; `session` on a genuinely fresh install | Where the tabs you start in the Roost window run. See [the dedicated section below](#local-backend). |

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

- **macOS: Cmd** — matches the Swift app and native Mac apps (⌘-click).
- **Linux: Alt** — Roost's single "primary" modifier on Linux, leaving
  Ctrl free for the shell/readline.

**Linux users who prefer the traditional Ctrl+click** (the ghostty /
common-terminal convention) just set:

```conf
link-modifier = ctrl
```

> **Scope:** this setting is honored by the **iced UI** — the shipped
> Linux `roost` and the experimental `Roost-Iced.app` alike
> (`link_modifier_held` in `crates/roost-iced/src/app/interactions.rs`).
> The Swift Mac app's modifier is currently fixed to Cmd, so the key is
> silently ignored there (harmless — unknown keys are always dropped).
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

## `agent-hooks`

Which supported coding agents Roost wires its hook entries into, in
their own config files. Both UIs read this at startup, and `roostctl
agent ensure` (the same verb the UIs run) reads it too — see the [Agent
Hooks](../guides/agents.md) guide for the full mechanism, what gets
written where, and the guarantee about what a merge does and doesn't
touch.

```conf
agent-hooks = claude, codex   # a list, off, or absent (nobody has answered yet)
```

| Value | Effect |
|---|---|
| a comma list of agent names | Every named agent gets Roost's hook entries, refreshed on launch if they're stale. Everything present and *not* named is left alone — nothing is ever removed just for being absent from the list. |
| `off` — also accepts `false`, `no` | The UIs wire nothing at startup; an agent's config file is never opened. This does **not** remove anything already wired on its own — see below. |
| absent, empty, or unrecognized | **Unanswered.** Nobody has run the consent dialog or `roostctl agent set` yet. Nothing is wired, and nothing already wired is touched, until they do — a fresh install starts here, and a launch raises the consent dialog once per process when at least one supported agent is installed. See [Agent Hooks → Roost asks once](../guides/agents.md#roost-asks-once). |

**Names normalise.** Each token is trimmed and lowercased; duplicates
collapse; the result is reordered into `claude, codex, grok, cursor,
opencode` regardless of the order written. A token that names none of
the five is dropped with a warning, and the rest of the list still
resolves — one typo shouldn't turn into "nothing is wired and nothing
says why." A value that names *no* recognised agent at all, or mixes a
reserved word (`off`/`false`/`no`, or the retired `auto`/`on`/`true`/
`yes`) with anything else, resolves to **unanswered** with a warning —
the retired spellings no longer mean "wire everything"; they mean
"nobody has said yet." A repeated key is last-wins, including when the
repeat is empty or unparseable (it resolves to unanswered, not to
whatever an earlier line in the file said).

**`off` stops future wiring; it does not itself remove anything.** This
split is deliberate, not an oversight: flipping the key to `off` and
relaunching means the UI won't wire anything *new*, but entries already
on disk stay there until something explicit takes them out —
`roostctl agent uninstall --all`, running `roostctl agent ensure`
yourself (which reads this same key and, seeing `off`, does perform the
removal), or reopening the consent dialog (the **Agent Hooks…** palette
row) and choosing `off` there. The UIs themselves never rewrite an
agent's config file just because the key changed to `off` — they simply
stop touching it.

**Over a connected host session the rule is different again, and
reversed from an earlier design.** A connecting client can only ever
**raise** a host's `agent-hooks` key — union its own allow-list into
whatever the host's key already says, never removing — and that
includes raising a host whose key is explicitly `off` or still
unanswered: a host has no screen to ask on, so the connecting client is
the only authority there is. A client whose own key is `off` or
unanswered sends nothing and so cannot raise or lower a host either.
**Lowering a host is done on that machine** — `roostctl agent
ensure`/`uninstall` run there, or its own dialog — and holds until a
more permissive client connects again. See [Agent Hooks → Remote
hosts](../guides/agents.md#remote-hosts) for the full behavior.

The Mac app carries its own mirror of this parser (`AgentHooks` in
`Config.swift`) rather than reading it only through `roostctl` — it
needs the key at launch to decide whether to raise the consent sheet
before it has spawned anything — kept in parity with the Rust parser,
including the normalisation rules, by `ConfigAgentHooksTests`.

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

Read by the iced UI. The Swift Mac app (`Roost.app`) is always
in-process and never reads this key.

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
# (Cmd on Mac, Alt on Linux). iced UI only — the Swift Mac app is
# always Cmd-click.
link-modifier = ctrl

# Add a custom trigger (here, restoring the pre-Alt Ctrl+T for new_tab
# on Linux — the default is now Alt+T). See docs/getting-started/keybindings.md.
keybind = ctrl+t = new_tab

# A fixed launcher command (⌘⇧T / Alt+Shift+T):
command = label="Claude" run="claude --resume"

# A dynamic, script-backed menu (⌘⇧E / Alt+Shift+E). The script prints
# its rows on `list` and acts on the choice on `activate`:
provider = label="Open shed" run="~/.config/roost/providers/shed.sh"

# Left absent (the default), Roost asks once — a consent dialog opens
# on first launch naming whichever of Claude Code, Codex, grok/gx,
# cursor-agent and OpenCode it finds installed, and nothing is wired
# until you answer it. Answering by hand instead: name the agents to
# wire, or turn the whole thing off (which stops future wiring — it
# doesn't by itself remove what's already there; see
# docs/guides/agents.md).
agent-hooks = claude, codex
```

See [Extending Roost](../guides/extending.md) for the full `command =` /
`provider =` contract (with bash / Python / TypeScript examples),
[`paths.md`](paths.md) for where the file lives on each platform,
[`themes.md`](themes.md) for the `theme` value enumeration, and the
[Agent Hooks](../guides/agents.md) guide for `agent-hooks` in full.
