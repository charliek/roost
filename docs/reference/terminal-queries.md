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
- **CSI/DCS device queries** (DA1/DA2, DSR, XTVERSION, ENQ, DECRQM, size
  reports) — libghostty-vt **does** answer these, via the `write_pty`
  effects callback. The answers are pure VT bookkeeping (cursor position,
  mode state, a static attribute string) that the engine already tracks.
  These replies are **libghostty-answered**.

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

## Channel 2 — libghostty-answered device queries (`write_pty`) — *planned*

> **Status: not yet implemented.** The OSC color channel above is live; this
> device-query channel is the planned follow-up. Until it lands, Roost
> answers only the OSC color channel, so probing TUIs see a terminal that
> ignores DA1/DSR/XTVERSION/DECRQM. (Note: this does *not* affect opencode —
> it falls back gracefully without device-query replies.) The rest of this
> section describes the intended design.

libghostty-vt's C terminal layer installs trampolines for an effects
callback set called `write_pty`. When the parser produces a host
response — DA1/DA2 (`ESC[c` / `ESC[>c`), DSR (`ESC[5n` / `ESC[6n`),
XTVERSION (`ESC[>q`), ENQ, DECRQM mode reports (`ESC[?Ps$p`), size
reports — it hands the bytes to `write_pty`. Roost *would* wire that
callback and forward the bytes to the PTY input.

The callback fires **synchronously inside `vt_write`** (mid-parse), so the
plan is a **collect-then-send** buffer: the callback only appends reply
bytes to a per-tab buffer, and Roost drains that buffer to `send_input` /
`onKey` *after* `vt_write` returns. This avoids any reentrancy/borrow
hazard and keeps replies FIFO-ordered with keystrokes.

**Deferred:** DEC mode 2031 (live light/dark color-scheme change
notifications) is tracked separately — it needs Roost to *proactively*
emit a DSR when its theme switches at runtime, which is more than wiring
the reply callback. See the DEC 2031 issue.
