# Terminal query replies

A program running in a tab can *query* the terminal for state — "what's
your background color?", "what are your device attributes?", "is
synchronized output on?". The terminal answers in-band by writing bytes
back onto the PTY's input. Many TUIs gate features (or whole render
paths) on these replies, so Roost has to answer the ones that matter.

Roost answers queries through **two distinct channels**, split by *who
owns the answer*. The split is deliberate and load-bearing — get it
wrong and you either double-answer or leave a query unanswered.

## Why two channels

`libghostty-vt` parses the byte stream for screen state, but it does
**not** answer every query itself:

- **OSC color queries** (`OSC 4` palette, `OSC 10/11/12` fg/bg/cursor) —
  libghostty-vt **no-ops the `.query` arm** of its color handler in every
  version (`src/terminal/stream_terminal.zig`, the `.query => {}` arm).
  It can't, really: the *embedder* (Roost) owns the palette and theme, so
  only Roost knows the right RGB to report. These replies are
  **embedder-synthesized**.
- **CSI/DCS device queries** (DA1/DA2/DA3, DSR 5n/6n, XTVERSION, DECRQM,
  the Kitty keyboard query, mode-2048 resize reports) — libghostty-vt
  **does** answer these autonomously, via the `write_pty` effects
  callback. The answers are pure VT bookkeeping (cursor position, mode
  state, a static attribute string) that the engine already tracks. These
  replies are **libghostty-answered**. (A second class — ENQ, XTWINOPS
  14/16/18t size reports, DSR `?996` — needs *additional* provider
  callbacks that `write_pty` alone does not enable; see Channel 2's
  "provider-gated set". Those remain unanswered, deferred to #209.)

The two sets are disjoint, so the channels never overlap or double-reply.

> **No Ghostty bump is needed for either.** OSC colors are embedder-owned
> in all Ghostty versions (a bump can't make libghostty answer them), and
> the `write_pty` device-reply set is already complete at the pinned SHA.

## Channel 1 — embedder-synthesized OSC color replies

Roost runs its own OSC scanner over the PTY output, *in parallel* with
feeding the same bytes to libghostty (`roost-osc` crate on Linux,
`OscScanner.swift` on macOS). The scanner surfaces query events; the UI
answers them and writes the reply onto the same per-tab PTY-input channel
as keystrokes (`TabSession::send_input` on Linux, `onKey` on macOS), so
the reply is FIFO-ordered with other input once enqueued.

| Query | Scanner event | Reply formatter | Live data source |
|---|---|---|---|
| `OSC 10/11/12 ;?` | `ColorQuery(n)` | `format_color_query_response` | `Terminal::live_colors` (theme fallback) |
| `OSC 4 ;Ps;?` | `PaletteQuery([Ps])` | `format_palette_query_response` | `Terminal::live_palette` (theme fallback) |

The data source reads libghostty's **live** colors/palette
(`ghostty_terminal_get`), so a mid-session `OSC 4;Ps;rgb:…` /
`OSC 11;rgb:…` *set* (which libghostty applies and the scanner ignores)
is reflected in the next query reply. If the FFI read fails, Roost falls
back to the static theme color/palette.

**Why OSC 4 matters:** opencode's TUI (`@opentui/core`) gates *all* of
its terminal color detection behind a single probe — `OSC 4;0;?` with a
300 ms timeout. If that goes unanswered it returns an all-`null` palette
and opencode renders an unreadable gray fallback theme. Answering OSC 4
is what unblocks it (and any other opentui-based TUI).

Code: `crates/roost-osc/src/lib.rs` (scanner + formatters),
`crates/roost-linux/src/app.rs` (drain reply arm),
`mac/Sources/Roost/TerminalView.swift` (`appendBytes` reply arm).

## Channel 2 — libghostty-answered device queries (`write_pty`) — *live*

> **Status: live (#247).** Roost installs libghostty-vt's `write_pty`
> effects callback on both UIs, so the engine-autonomous device replies
> below now reach the PTY. Before this, probing TUIs saw a terminal that
> ignored DA1/DSR/XTVERSION/DECRQM — most visibly crossterm's
> `supports_keyboard_enhancement()` blocked ~2 s on an unanswered Kitty
> query and then never pushed Kitty flags (so Shift+Enter arrived as a
> bare `\r`).

libghostty-vt's C terminal layer answers a set of CSI/DCS device queries
itself, handing the response bytes to the `write_pty` effects callback.
Roost wires that callback and forwards the bytes onto the same per-tab
PTY-input channel as keystrokes.

### Answered — engine-autonomous set (enabled by `write_pty` alone)

| Query | Sequence | Reply (engine default at the pinned SHA) |
|---|---|---|
| DA1 / DA2 / DA3 | `ESC[c` / `ESC[>c` / `ESC[=c` | e.g. DA1 → `ESC[?62;22c` (VT220 + ANSI color) |
| DSR status | `ESC[5n` | `ESC[0n` ("OK") |
| DSR cursor position (CPR) | `ESC[6n` | `ESC[<row>;<col>R` |
| DECRQM mode report | `ESC[?Ps$p` | `ESC[?Ps;<state>$y` |
| Kitty keyboard query | `ESC[?u` | `ESC[?<flags>u` (e.g. `ESC[?0u` with no flags pushed) |
| XTVERSION | `ESC[>q` | `DCS >\|libghostty ST` (the literal default; roost ships no override) |
| CSI 21t title report | `ESC[21t` | the current title (from libghostty's `getTitle`) |
| Mode-2048 in-band resize report | fires on `resize` when mode 2048 is set | `ESC[48;<rows>;<cols>;<h>;<w>t` |

### NOT answered — provider-gated set (deferred to #209)

These need *separate* libghostty callbacks that don't exist yet, so
`write_pty` alone does not enable them. They are deliberately out of scope
(the `write_pty` design owns `OPT_USERDATA` exclusively — see below):

| Query | Sequence | Missing callback |
|---|---|---|
| ENQ | `0x05` | `enquiry` (empty default → early return) |
| XTWINOPS size reports | `ESC[14t` / `ESC[16t` / `ESC[18t` | `size` (`orelse return`) |
| DSR color-scheme | `ESC[?996n` | `color_scheme` |

### Drain shape — collect-then-send, after `vt_write` **and** `resize`

The callback fires **synchronously inside `vt_write`** (mid-parse) **and
inside `resize`** — mode 2048 (in-band size reports) emits its report from
`ghostty_terminal_resize`, *outside* `vt_write`. So Roost uses a
**collect-then-send** buffer: the callback only appends reply bytes to a
per-tab buffer, and Roost drains that buffer to `send_input` / `onKey`
*after* the producing call returns. Draining only after `vt_write` would
silently drop resize-triggered reports.

Drain points, per UI:

- **macOS** (`mac/Sources/Roost/TerminalView.swift`): the module-level
  `roostWritePtyCallback` appends to `pendingPtyReplies`;
  `flushPendingPtyReplies()` drains it into `onKey` at the end of
  `appendBytes` (post-`vt_write`) and immediately after the
  `ghostty_terminal_resize` call in `reflowGridForBounds`.
- **Linux** (`crates/roost-linux/src/terminal_view.rs`): the trampoline
  (in `crates/roost-vt/src/terminal.rs`) appends to an
  `Arc<Mutex<Vec<u8>>>`; `TerminalView::drain_write_pty` takes the bytes
  and dispatches through `input_callback` after `TerminalView::vt_write`
  and after every `reflow` (each of the three resize call sites).

The buffer take never holds its lock across `vt_write`/`resize` (the
trampoline locks the same mutex synchronously — a held guard would
self-deadlock), and the callback is append-only (it must never re-enter
`vt_write` per libghostty's no-reentrancy contract).

**`OPT_USERDATA` exclusivity.** libghostty exposes a *single* shared
userdata slot for **all** of its callbacks. The `write_pty` wiring claims
it exclusively; adding any second callback (bell, title, size, enquiry,
color-scheme, …) later can't just set its own userdata — it must be
multiplexed through one shared context struct. That refactor is deferred
until a second callback actually exists (#209 remainder).

**Cross-channel ordering nuance.** Within a *single mixed chunk*, Channel
1 (OSC) replies are synthesized *before* `vt_write` while Channel 2
(device) replies drain *after* it, so replies from one chunk are not
ordered by byte position across the two channels. This is harmless — the
query sets are disjoint and each channel is internally ordered.

**Deferred:** DEC mode 2031 (live light/dark color-scheme change
notifications) is tracked separately — it needs Roost to *proactively*
emit a DSR when its theme switches at runtime, which is more than wiring
the reply callback. The DSR `?996` *query* likewise stays unanswered
(needs the `color_scheme` provider callback). See the DEC 2031 issue.
