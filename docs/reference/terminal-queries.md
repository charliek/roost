# Terminal query replies

A program running in a tab can *query* the terminal for state — "what's
your background color?", "what are your device attributes?", "is
synchronized output on?". The terminal answers in-band by writing bytes
back onto the PTY's input. Many TUIs gate features (or whole render
paths) on these replies, so Roost has to answer the ones that matter.

**libghostty-vt answers every query Roost answers.** Both the OSC color
queries and the CSI/DCS device queries come back through one channel —
libghostty's `write_pty` effects callback, which Roost installs on both
UIs and drains onto the tab's PTY-input channel. Roost synthesizes
nothing.

## Who owns the answer

That was not always true, and the history is worth keeping because the
failure mode is loud:

- **OSC color queries** (`OSC 4` palette, `OSC 10/11/12` fg/bg/cursor,
  and Kitty's `OSC 21`) — libghostty-vt used to **no-op the `.query`
  arm** of its color handler (`src/terminal/stream_terminal.zig`), so
  Roost scanned for those queries itself and wrote its own replies.
  Upstream `14c829883` ("terminal: report OSC color queries in lib-vt",
  in Roost as of the pinned SHA `f2d5758f6`) makes libghostty answer
  them from its own color state whenever `write_pty` is installed —
  which Roost does, for the device replies below. Roost's synthesis
  became a **second** answer to every query, so it was removed.
- **CSI/DCS device queries** (DA1/DA2/DA3, DSR 5n/6n, XTVERSION, DECRQM,
  the Kitty keyboard query, mode-2048 resize reports) — libghostty-vt
  has always answered these autonomously through the same callback. (A
  third class — ENQ, XTWINOPS 14/16/18t size reports, DSR `?996` — needs
  *additional* provider callbacks that `write_pty` alone does not
  enable; see the "provider-gated set" below. Those remain unanswered,
  deferred to #209.)

> **The pin is load-bearing for OSC colors.** Before `f2d5758f6` these
> queries went unanswered without embedder synthesis; after it, embedder
> synthesis double-answers. A Ghostty SHA bump that moved backwards past
> that commit would silence them.

## Where the colors come from

libghostty resolves a color query as `override orelse default`:
`override` is whatever an `OSC 4;Ps;rgb:…` / `OSC 11;rgb:…` *set* from
the program established, and `default` is the theme Roost pushed through
`ghostty_terminal_set(GHOSTTY_TERMINAL_OPT_COLOR_*)` at tab creation and
on every theme switch (`TerminalTab::apply_theme_candidate` on iced,
`Theme.apply` on macOS). **That push is what makes the queries
answerable at all** — a terminal with neither an override nor a default
has nothing to report and stays silent (pinned by
`an_unseeded_color_query_gets_no_reply` in
`crates/roost-vt/tests/write_pty_test.rs`).

Replies use the fixed 16-bit-per-channel xterm form
(`ESC]11;rgb:1e1e/1e1e/1e1e BEL`), preserve the terminator the request
used (BEL or ST), and fall back from cursor to foreground when no cursor
color is set. Semantics are **sequential**: requests are processed in
wire order, so a SET earlier in the same write is visible to a QUERY
behind it. (Roost's old drain-side replies answered such a query from a
chunk-start snapshot — the *pre-chunk* color. That divergence is gone
with the synthesis.)

**Why OSC 4 matters:** opencode's TUI (`@opentui/core`) gates *all* of
its terminal color detection behind a single probe — `OSC 4;0;?` with a
300 ms timeout. If that goes unanswered it returns an all-`null` palette
and opencode renders an unreadable gray fallback theme.

**What Roost's own OSC scanner still does.** Roost keeps scanning PTY
output in parallel with libghostty (`roost-osc` on Linux,
`OscScanner.swift` on macOS) — it is how title / cwd / notification /
clipboard / pointer-shape OSCs reach the workspace, since libghostty's
C API surfaces no payload for them. The scanner surfaces color queries
too, but every consumer now drops them; `roost-engine`'s `OscColorState`
tracks only the SET/RESET forms, as the drain's own record of a tab's
colors.

Code: `crates/roost-osc/src/lib.rs` (scanner),
`crates/roost-engine/src/osc.rs` (`OscRouter` — color events produce no
action), `mac/Sources/Roost/TerminalView.swift` (`appendBytes` passes
color queries through). Reply bytes are pinned at the FFI in
`crates/roost-vt/tests/write_pty_test.rs`, end-to-end on macOS in
`mac/Tests/RoostTests/TerminalViewOscDrainTests.swift`, and on both UIs
by `tools/roosttest/test_osc_pipeline.py`.

## The `write_pty` channel — *live*

> **Status: live (#247).** Roost installs libghostty-vt's `write_pty`
> effects callback on both UIs, so the engine-autonomous device replies
> below now reach the PTY. Before this, probing TUIs saw a terminal that
> ignored DA1/DSR/XTVERSION/DECRQM — most visibly crossterm's
> `supports_keyboard_enhancement()` blocked ~2 s on an unanswered Kitty
> query and then never pushed Kitty flags (so Shift+Enter arrived as a
> bare `\r`).

libghostty-vt's C terminal layer answers these queries itself, handing
the response bytes to the `write_pty` effects callback. Roost wires that
callback and forwards the bytes onto the same per-tab PTY-input channel
as keystrokes.

### Answered — engine-autonomous set (enabled by `write_pty` alone)

| Query | Sequence | Reply (engine default at the pinned SHA) |
|---|---|---|
| OSC color query | `ESC]10;?` / `ESC]11;?` / `ESC]12;?` | `ESC]11;rgb:1e1e/1e1e/1e1e BEL` (request's terminator preserved) |
| OSC palette query | `ESC]4;Ps;?` | `ESC]4;0;rgb:RRRR/GGGG/BBBB BEL` |
| Kitty color query | `ESC]21;foreground=?` | terminal-backed keys reported; unset dynamic colors report empty |
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
- **iced** (`crates/roost-iced/src/app/terminal_tab.rs`): the trampoline
  (in `crates/roost-vt/src/terminal.rs`) appends to the
  `Arc<Mutex<Vec<u8>>>` installed by `Terminal::set_write_pty_buffer`;
  `TerminalTab::drain_terminal_replies` takes the bytes and hands them to
  `TabSession::send_input` after `write_vt`. The resize path is staged
  rather than sent inline: `apply_geometry` takes the buffer into
  `GeometryChange::deferred_replies`, and those bytes are sent only once
  the geometry transaction commits — a rolled-back resize discards its
  report, because the PTY never observed that size.

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

**Ordering.** Every reply — OSC color and CSI/DCS device alike — is
emitted from inside `vt_write` (or `resize`) in wire order and drained
after the producing call returns, so one chunk's replies leave in the
order its queries arrived. Roost adds nothing ahead of them.

### DEC 2031 — proactive color-scheme change notification

Mode 2031 (`CSI ? 2031 h`) is an app *opt-in* to be told when the
terminal's color scheme flips between light and dark at runtime. Unlike
the queries above, nothing on the PTY output triggers it — it's
**proactive**: when the user switches Roost's theme mid-session, Roost
writes a DSR onto the PTY input of every tab whose terminal has mode 2031
enabled:

| New scheme | Emitted |
|---|---|
| dark | `CSI ? 997 ; 1 n` (`ESC[?997;1n`) |
| light | `CSI ? 997 ; 2 n` (`ESC[?997;2n`) |

Light vs dark is derived from the theme's **background luminance** (Rec.
709 weighted sum over the sRGB channels, > 0.5 → light) — the same one
formula on both UIs (`ColorRgb::is_light` in
`crates/roost-vt/src/render_state.rs`; `Theme.isLight` in
`mac/Sources/Roost/Theme.swift`). Roost ships only dark themes today, so
in practice the report is `997;1n` until a light theme is added.

The report is emitted from each UI's runtime theme-switch path
(`TerminalTab::set_theme` in the iced UI, `TerminalView.setTheme` on
macOS),
routed through the **same per-tab PTY-input sink as keystrokes** (`onKey`
/ `input_callback` → `send_input`) so it stays FIFO-ordered with input
and is visible to `tab.capture_pty_input`. Gating: only when the tab's
terminal reports mode 2031 set (`ghostty_terminal_get(DATA_MODE)`), and
only on an actual theme switch — merely *enabling* the mode emits nothing
(an app that wants the current state queries `?996`).

**Still deferred (#209):** the DSR `?996` *query* (`ESC[?996n`, "what
scheme are you now?") stays unanswered — it needs the `color_scheme`
provider callback, which conflicts with the buffer-only `OPT_USERDATA`
design (see the exclusivity note above). So mode 2031 gives apps live
*change* notifications, but a cold *query* of the current scheme is not
yet answered.
