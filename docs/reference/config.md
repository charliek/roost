# User config

Roost reads a single user-level config file at `~/.config/roost/config.conf`
(XDG-style on macOS by deliberate divergence from Apple HIG — matching
Ghostty / nvim / fish). Both UIs (Swift Mac app, Linux gtk4-rs binary)
parse the same file with the same semantics, so a config tuned on one
platform is portable to the other.

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
| `font-family` | string | platform default | Monospaced font family. Quoted values supported (`"JetBrains Mono"`). |
| `font-size` | number | `13` (Linux), `14` (Mac) | Point size for the terminal font. Must be `> 0`. |
| `tab-min-width` | number | `80` (Mac) | Minimum tab pill width in points. `0` disables the floor. Mac-only. |
| `tab-max-width` | number | `220` (Mac) | Maximum tab pill width in points. `0` disables the cap (pills grow to fit). Mac-only. |
| `keybind` | `<trigger> = <action>` | (see `cmd/roost/shortcuts.go` legacy notes) | Append a custom keybinding. Repeatable; later entries override earlier ones. |
| `command` | `label="…" run="…" [hold=…]` | none | Launcher entry surfaced in the command palette. Repeatable. |
| `copy-on-select` | `off | true | clipboard` | `true` | What a mouse-drag selection writes to the clipboard on release. See [the dedicated section below](#copy-on-select). |
| `clipboard-write` | `allow | deny` | `allow` | Whether a program running in the terminal can write the host clipboard via OSC 52. See [the dedicated section below](#clipboard-write). |

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

### Trimming

The text written to either clipboard has trailing whitespace stripped
per row (matches Ghostty's `clipboard-trim-trailing-spaces` default
behavior). Leading/trailing entirely-blank rows are also dropped so
multi-row selections don't carry stray newlines.

### Limitation (v1)

If a selection extends beyond the visible viewport (the user drags
inside the visible area, then scrolls so the selection is partly or
fully off-screen), copy returns only the still-visible portion. The
visible highlight rectangle scrolls with the content, so the visual
contract is correct — but the copied text is what's currently shown,
not the entirety of the original selection. A future PR will add
scroll-walk-restore to copy off-screen rows; the bug-fix
[#146](https://github.com/charliek/roost/pull/146) explicitly leaves
this as a known limitation.

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

### Targets

OSC 52 carries a `Ps` selector indicating which clipboard to write:

- `c` (default) → system clipboard (`NSPasteboard.general` on Mac,
  `CLIPBOARD` on Linux — what ⌘V / Ctrl+V pastes from).
- `p` or `s` → selection clipboard (named `NSPasteboard` on Mac,
  X11 / Wayland `PRIMARY` on Linux — what middle-click pastes from).
- Any other selector falls through to system (matches Ghostty's
  permissive handling of emitters that pad the selector with letters).

## Example

```
# ~/.config/roost/config.conf

theme = catppuccin-mocha
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

keybind = ctrl+t = new_tab
command = label="Claude" run="claude --resume"
```

See [`paths.md`](paths.md) for where the file lives on each platform,
and [`themes.md`](themes.md) for the `theme` value enumeration.
