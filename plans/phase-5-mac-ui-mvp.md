# Phase 5: Mac UI MVP

**Status**: ✅ done
**Exit criteria** (all met):
* Single-window AppKit `.app` opens, performs an Identify handshake against `roost-core`, and displays connection state.
* libghostty-vt linked into the Roost SwiftPM target via a `CGhosttyVT` shim — `ghostty_terminal_new` / `ghostty_terminal_vt_write` / `ghostty_terminal_free` callable from Swift.
* `TerminalView: NSView` walks libghostty-vt's render state and draws per-cell backgrounds + glyphs (`NSAttributedString` per cell — simple-but-correct, glyph atlas deferred).
* Bidirectional `StreamPty` session: daemon's PTY output flows into the renderer; keystrokes captured by `TerminalView.keyDown` flow back over the stream.
* Special keys (arrows, Home/End, Page Up/Down, F1–F12, forward Delete) translated to xterm-style VT/CSI escapes (Phase 5.5c "lite").
* `swift build` + `swift test` green on macOS CI.
* User can type `ls`, `vim`, `bash` in the window backed by the daemon. **Architecture proof point.**

## Goal

Smallest functional native Mac UI built against the proto. This is the architecture proof point — once typing bash works end-to-end through the wire, the rest of the Mac story is "make it nicer" rather than "find out if it works."

## Scope

In:
* AppKit window + `TerminalView` renderer.
* libghostty-vt FFI via the `CGhosttyVT` SwiftPM target.
* grpc-swift v2 client over UDS (with `:authority = "localhost"` — see below).
* Special-key translation table.

Out:
* Multi-tab / multi-project (Phase 6a).
* OSC parsing (Phase 6b).
* Glyph atlas / Metal renderer (out of scope until the simple renderer profiles painful).
* Selection, copy/paste (Phase 6a/6b polish).
* Font configuration (Phase 6a polish or later).

## Touches Go code?

No. New SwiftPM package in `mac/`, new `CGhosttyVT` shim, no edits to the Go binary.

## Commits

Landed across the Phase 5 series:

* `5b16500` — Phase 5 step 1: Mac AppKit window skeleton.
* `f4d0f6c` — Address CodeRabbit follow-ups on the AppKit skeleton.
* `054f368` — Phase 5 step 2: Mac UI runs Identify() against roost-core over UDS.
* `5698408` — Make Mac UI compile against grpc-swift v2's actual API surface.
* `5dbcb39` — Fix swift-mac: PROTOC_PATH env + corrected plugin config schema.
* `54e271a` — Pass --disable-sandbox to swift build so the GRPCProtobufGenerator plugin can invoke `protoc`.
* `e1fd984` — Restore correct SwiftPM plugin config filename.
* `b2e6540` — Phase 5 step 3 (Mac side): wire libghostty-vt into the SwiftPM build.
* `5aa981d` — Fix CGhosttyVT include resolution: switch from systemLibrary to .target.
* `d92d3c5` — Address CodeRabbit batch + fix CGhosttyVT include path.
* `fb1e1c4` — Fix CGhosttyVT smoke: GhosttyResult is a wrapper struct, compare via .rawValue.
* `c7f268d` — Mac libghostty-vt: link the static archive positionally to avoid dyld @rpath.
* `f85d43b` — Phase 5 step 4a: TerminalView NSView with libghostty-vt lifecycle.
* `cf373a2` — Mark TerminalView.terminal nonisolated(unsafe) for Swift 6 deinit.
* `a721c84` — Phase 5 step 4b: render-state lifecycle + canvas color from libghostty-vt.
* `2cb496e` — Phase 5 step 4c: per-cell background fill + ANSI demo write.
* `c594581` — Phase 5 step 4d: glyph rendering via NSAttributedString.
* `4a8c5fb` — Phase 5 step 5a: read-only StreamPty session — daemon PTY → Mac renderer.
* `6370ffd` — Phase 5 step 5b: keystroke routing — type into the Mac UI hits the daemon shell.
* `df19074` — Mark RoostApp @MainActor so [weak self] survives @Sendable boundaries.
* `30ee267` — Decouple StreamPty output from self via AsyncStream bridge.
* `f790929` — Phase 5.5c-lite: arrow / nav / function keys via direct VT escapes.

The big subsequent fix:

* `4a7cf4c` — **Fix Mac UI Identify (RST_STREAM PROTOCOL_ERROR over UDS).** First live test exposed that grpc-swift-nio-transport defaults `:authority` to the socket path, which tonic's h2 rejects. Pass `authority: "localhost"` explicitly. (See Decision Log addendum at the bottom of this doc.)
* `82f1237` — Mac CI: live-daemon Identify regression guard. CI now starts `roost-core` and runs an Identify round-trip in `swift test` to catch this class of regression.

## Risks / known gaps

* The renderer is simple — `NSAttributedString.draw` per cell. Acceptable for typing speeds; profile if real workloads regress.
* No selection / copy. Phase 6a polish or later.
* No font configuration. Phase 6a polish or later (the Go binary's `~/.config/roost/config.conf` font block doesn't have a Swift equivalent yet).
* Glyph wide chars (CJK, emoji) probably misalign. The cell-metric math assumes monospace single-cell glyphs; libghostty-vt's render state knows the width but the Mac walker doesn't consume the width field yet.

## Decision log addendum (Phase 5)

### DL-11: UDS `:authority` is `"localhost"`

`grpc-swift-nio-transport`'s UDS resolver defaults `:authority` to the raw socket path when none is given, and tonic's underlying `h2` rejects authorities containing `/` (RFC 3986). The ecosystem convention over UDS is the literal string `"localhost"`. Roost's Mac client passes that explicitly via `.unixDomainSocket(path: socketPath, authority: "localhost")`. Without this, every RPC fails with `RST_STREAM(PROTOCOL_ERROR)` at the HTTP/2 framing layer — confirmed cause, canonical fix, refs in commit `4a7cf4c`'s message.

## Follow-ups

* Phase 5.5c-full would replace the lite arrow-key table with libghostty-vt's full key encoder (`ghostty_key_encoder_*`) — adds modifier support (Shift+Arrow, Option+Left for word jump), kitty keyboard mode, IME composing state. Currently scheduled after Phase 7 unless a user-visible issue forces it earlier.
* The simple `NSAttributedString` renderer is acceptable through Phase 6; Metal/Core Text glyph atlas only worth it if profiling shows the per-cell allocation in `draw()` is the dominant cost.
