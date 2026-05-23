# Goal — Mac polish: cursor, keys, sidebar, secondary shortcuts, exit cascade, scrollback

**Set**: 2026-05-17
**Owner**: Charlie Knudsen
**Co-author / executor**: Claude (Opus 4.7)
**Status**: ✅ closed — all M1–M6 landed on `feature/rust-port` by 2026-05-17, plus three post-goal fixes (orphan-tab purge, cursor-on-focus override, OSC UTF-8 multibyte) and the Swift OSC + KeyEncoder test additions.
**Predecessors**: [`goal-rust-port-polish-2026-05-16.md`](goal-rust-port-polish-2026-05-16.md), [`goal-phase-6-complete-2026-05-16.md`](goal-phase-6-complete-2026-05-16.md). Both closed; this goal is the next polish push.

## Problem

End-to-end testing on the Mac after Phase 6 closed surfaced six quality-of-life gaps where the Swift UI lags the Go binary on `main`:

1. **Shell `exit` is a no-op**. Go binary cascades shell exit → close tab → (if last tab) delete project → (if last project) quit. Swift today receives `PtyServerKind::Exit` on the StreamPty stream but doesn't fire `TabDeletedEvent`; on explicit close it auto-opens a fresh tab instead of cascading ([`mac/Sources/Roost/App.swift:595`](../mac/Sources/Roost/App.swift)).
2. **No cursor is rendered**. `TerminalView.draw(_:)` paints backgrounds, glyphs, and the selection overlay — but never draws the cursor. libghostty-vt's render state exposes everything we need ([`third_party/ghostty/out/include/ghostty/vt/render.h`](../third_party/ghostty/out/include/ghostty/vt/render.h)); the Swift wrapper just hasn't queried it.
3. **No sidebar toggle**. `toggle_sidebar` is defined and bound to ⌘B but no handler exists. Go force-opens the sidebar on `newProject` / `switchProjectByIndex` / `beginRenameActiveProject`.
4. **Shift+Tab and Shift+Enter are dropped**. `TerminalView.keyDown` falls through to `event.characters` which strips modifiers; Claude Code's mode-switcher (`\x1b[Z`) and multi-line input are unusable. Go delegates to libghostty-vt's key encoder ([`internal/ghostty/key.go`](../internal/ghostty/key.go)).
5. **Secondary shortcuts not wired**. `cycle_tab_prev` / `cycle_tab_next` (bound, no handler), `rename_tab` (defined, unbound, no handler). `toggle_sidebar` is the same as item 3.
6. **No scrollback**. The Swift terminal is built with `opts.max_scrollback = 0` ([`mac/Sources/Roost/TerminalView.swift:170`](../mac/Sources/Roost/TerminalView.swift)) — libghostty-vt isn't keeping history. Even if it were, no `scrollWheel(with:)` handler is wired. Go keeps 2000 rows ([`cmd/roost/session.go:186`](../cmd/roost/session.go)) and dispatches wheel events through three modes.

Out of scope (named follow-ups, not this goal): wide-char width handling, IME composition handling, selection-tracks-scroll across scroll/rotate, `polish/scrollback-keybinds` (keyboard scroll actions).

## Decision

Six milestones, one `polish/*` branch each, squash-merged to `feature/rust-port` with auto-merge gated on the 3 macOS-required CI checks. Same process as the predecessor goals.

| Question | Choice |
|---|---|
| Branch model | `polish/*` topic branches → `feature/rust-port` (same as predecessors). |
| CI gates | macOS-only required: `test (macos-latest)`, `rust-build (macos-latest)`, `swift-mac`. Linux jobs informational. |
| Ordering | M1 first (M6 depends on it for snap-on-keystroke). M2/M3/M4/M5/M6 independent — open in parallel as PRs stack. |
| Cadence | Milestone-sized PRs, mirroring P1–P9. |
| User-confirmed behavior | Last-tab → silently delete project row (Go `app.go:807-808`); last-project → close window, app quits (Go `app.go:2107-2115`). |

### M0 — Goal doc + branch setup (this commit)

Land this goal doc on `feature/rust-port`. Spin up `polish/key-encoder` for M1.

### M1 — libghostty-vt key encoder bridge

Port `internal/ghostty/key.go` to Swift. New `mac/Sources/Roost/KeyEncoder.swift` owns one `GhosttyKeyEncoder` per `TerminalView`, NSEvent → key/mods translation, `setopt_from_terminal` sync before each encode (terminal modes can change between keystrokes). Replaces `TerminalView.keyDown`'s hand-rolled byte path; deletes the `specialKeyBytes` table.

**Touches**: `mac/Sources/Roost/KeyEncoder.swift` (new), `mac/Sources/Roost/TerminalView.swift` (replace key path).
**Reference**: `../ghostty/macos/Sources/Ghostty/NSEvent+Extension.swift:12-48`, `../ghostty/macos/Sources/Ghostty/Surface View/SurfaceView_AppKit.swift:~900-1000`. C API: `third_party/ghostty/out/include/ghostty/vt/key/{encoder,event}.h`. Go binding: `internal/ghostty/key.go`.

### M2 — Cursor rendering

Expose cursor getters in `RenderState.swift` (position, visibility, blinking, visual style, color). Add focus-aware blink timer (530ms, paused while window inactive, snaps to "on" on focus regain). Extend `draw(_:)` to draw block / bar / underline / hollow per `cursorVisualStyle` and focus state.

**Touches**: `mac/Sources/Roost/RenderState.swift`, `mac/Sources/Roost/TerminalView.swift`.
**Reference**: Go `cmd/roost/render.go:147-186`, `cmd/roost/session.go:77-82,310-319,1224-1235`. Ghostty Swift focus sync: `BaseTerminalController.swift:~450-465` + `SurfaceView_AppKit.swift:~650-680`.

### M3 — Sidebar toggle (⌘B) + auto-open + persistence

Implement `@objc func toggleSidebar(_:)` in `App.swift` against the existing `NSSplitView` setup at `App.swift:203-226`. Add `ensureSidebarVisible()`; call from `newProject` / `selectProject` / `beginRenameActiveProject` (mirroring Go `app.go:1337,1487,1975`). Persist `sidebarVisible` in `UserDefaults`. Wire a View menu item so the responder chain works.

**Touches**: `mac/Sources/Roost/App.swift`.
**Reference**: cmux's `SidebarState` (`../cmux/Sources/Sidebar/SidebarState.swift:1-17`), Go `cmd/roost/app.go:1301-1320`.

### M4 — Secondary shortcuts: cycle tab + rename tab

`cycleTabPrev` (⌘⇧[) / `cycleTabNext` (⌘⇧]) — wrap around at ends. `renameActiveTab` (⌘R) — `NSPopover` anchored to the tab pill, on commit fires `SetTabTitle` with `user: true`. Adds `KeybindAction.renameTab: ["\(projectMod)+r"]` to `defaultBindingsMac`.

**Touches**: `mac/Sources/Roost/App.swift`, `mac/Sources/Roost/Keybind.swift`.
**Reference**: Go `cmd/roost/app.go:1465` (cycleTab), `:1520-1610` (renameActiveTab).

### M5 — PTY-exit cascade

Daemon-side: detect PTY exit and call `workspace.delete_tab(tab_id)` after StreamPty drains, which emits `TabDeletedEvent`. Swift-side: remove auto-open-new-tab in `tabDeleted` handler; if remaining tabs in project = 0, fire `DeleteProject` silently. Handle `ProjectDeletedEvent`; if remaining projects = 0, close the window. Confirm `applicationShouldTerminateAfterLastWindowClosed` returns `true`.

**Touches**: `crates/roost-core/src/pty.rs`, `crates/roost-core/src/service.rs`, `crates/roost-core/src/workspace.rs` (verify), `mac/Sources/Roost/App.swift`, `mac/Sources/Roost/RoostApp.swift`.
**Reference**: Go cascade `cmd/roost/app.go:800-811` → `:1436-1452` → `:2055-2079` → `:2085-2127` (`win.Close()` at 2114).

### M6 — Scrollback (wheel + snap)

Enable `opts.max_scrollback = 2000` (parity with Go). Override `scrollWheel(with:)`:
- Mouse-tracking → encode wheel as button-4/5 through M1's encoder.
- Alt-screen alt-scroll → translate to Up/Down arrow presses.
- Primary-screen → call `ghostty_terminal_scroll_viewport`, set `scrolledBack`.

Smooth-scroll fractional accumulator (3 rows per discrete notch). Snap-to-bottom hook in M1's `KeyEncoder.encode(event)`: if `scrolledBack`, call `ghostty_terminal_scroll_viewport(terminal, .toBottom)` and clear the flag *before* running the encoder. Matches Go `input.go:67`.

**Touches**: `mac/Sources/Roost/TerminalView.swift`, `mac/Sources/Roost/KeyEncoder.swift` (snap hook), `mac/Sources/Roost/RenderState.swift` (verify viewport iterator).
**Reference**: C API `third_party/ghostty/out/include/ghostty/vt/terminal.h:170-171,209-243,1002`. Go `cmd/roost/session.go:776-900` (handleScroll), `:421-431` (event controller), `:102-112` (state).

## Keybind override semantics

P1 already shipped the Ghostty-style `keybind = trigger = action` parser + canonicalization. All four M3/M4 target actions (`toggle_sidebar`, `cycle_tab_prev`, `cycle_tab_next`, `rename_tab`) are already in `KeybindAction.knownStaticActions` ([`mac/Sources/Roost/Keybind.swift:62-67`](../mac/Sources/Roost/Keybind.swift)) — known but unhandled today. The instant M3/M4 add their handlers, `~/.config/roost/config.conf` overrides start working without further plumbing. Same for M5 (`close_tab` already exists). Only M6's optional scroll-keybind actions (`scroll_up_line`, etc.) would need new `knownStaticActions` entries; those are deferred to a `polish/scrollback-keybinds` followup.

## Verification

End-to-end after all six milestones land:

1. **M1 keys** — Shift+Tab pops Claude Code's mode menu. Shift+Enter adds a new line in Claude prompt. Option+ArrowLeft/Right word-jumps in bash readline. Ctrl+R triggers reverse-i-search.
2. **M2 cursor** — Visible block cursor blinking at ~530ms. Click outside → hollow outline, no blink. Click in → solid block returns immediately. Claude prompt's DECSCUSR-driven bar style applies.
3. **M3 sidebar** — ⌘B hides/shows; visibility persists across launches. Hidden → ⌘N opens sidebar to show new project; ⌘1 opens to show focused project; rename-project opens sidebar.
4. **M4 secondary shortcuts** — ⌘⇧] / ⌘⇧[ cycle tabs (wrap at ends). ⌘R opens rename popover on the active tab pill; after commit, shell-emitted OSC 1/2 no longer overwrites the title.
5. **M5 exit cascade** — `exit` in last tab → project row leaves sidebar. Last project → window closes, app quits.
6. **M6 scrollback** — `seq 1 5000` then trackpad-scroll up shows earlier numbers. Any keystroke snaps viewport back to bottom before the keystroke is delivered. vim/less wheel hits arrow-key behavior (alt-screen). Mouse-tracked apps receive wheel events. Trackpad smooth-scroll is row-quantized.

Smoke: `swift build` + `swift test` from `mac/`; `cargo build -p roost-core` + `cargo test -p roost-core`; `cargo fmt --all -- --check`; `cargo clippy --no-deps -- -D warnings`.

## Risks / known gaps

- **M1 modifier translation**. Option-as-Alt config (`GHOSTTY_KEY_ENCODER_OPT_MACOS_OPTION_AS_ALT`) defaults to "alt" for now; expose as config later. IME composition (Korean, Japanese) needs Ghostty's `markedText` pattern — out of scope, deferred to `polish/ime-composition`.
- **M2 cursor visual style**. `GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK_HOLLOW` is libghostty-vt's own "blurred" hint; the Swift side also draws hollow when blurred. Prefer libghostty's signal if it ever says hollow; else use focus state. Don't double-up.
- **M3 NSSplitView vs NSSplitViewController**. Existing code uses raw `NSSplitView`. If wrapping each side in `NSSplitViewItem` is disruptive, hand-roll `setPosition(_:ofDividerAt:)` show/hide.
- **M5 daemon-side cascade**. The exit hook must run *after* StreamPty has finished draining its output buffer to the UI, otherwise trailing output bytes race the tab-deletion event and disappear. Mirror Go's `pumpPTY` drain-before-close ordering.
- **M6 NSEvent scroll variants**. Trackpad / Magic Mouse / discrete wheel produce different deltas. Test all three on real hardware.

## Phase doc vs goal doc rationale

This is a goal doc, not a phase doc, on the same precedent as the two predecessor goals. Phase docs in this repo represent larger architectural arcs (6a structural, 6b OSC, 7 Linux, 8 bundling, 9 cutover); this is follow-up polish on closed Phase 6a / 6b. After this goal closes, the deferred-followup lines in [`plans/phase-6a-mac-structural.md`](phase-6a-mac-structural.md) and [`plans/goal-phase-6-complete-2026-05-16.md`](goal-phase-6-complete-2026-05-16.md) should get a "✅ landed via goal-mac-polish-cursor-keys-2026-05-17.md" addendum.

## Milestone log

* M0 — ✅ goal doc landed.
* M1 — ✅ libghostty-vt key encoder bridge — merged via PR #42 (`235ce43`).
* M2 — ✅ cursor rendering — merged via PR #43 (`f983cec`).
* M3 — ✅ sidebar toggle + auto-open + UserDefaults persistence — merged via PR #44 (`25ef079`).
* M4 — ✅ cycle prev/next tab + rename tab handlers — merged via PR #45 (`dc533f0`).
* M5 — ✅ PTY-exit cascade — merged via PR #46 (`38dce69`).
* M6 — ✅ scrollback — 2000-row buffer, wheel handler, snap-on-keystroke — merged via PR #47 (`d294cc6`).

### Post-goal fixes on `feature/rust-port` (2026-05-17)

These addressed dogfooding bugs surfaced after the goal's milestones landed; flagged here so the audit trail stays connected to this goal:

* `234378e` (PR #49) — Daemon orphan-tab purge at startup. Fixed the M5 cascade misfire where stale tab rows from prior daemon sessions persisted in SQLite and prevented project-delete cascades from firing.
* `5009a0c` (PR #48) — `KeyEncoder` keep UTF-8 buffer alive across `encode()` (fix dropped chars). M1 follow-up: the original M1 had a lifetime bug where `set_utf8`'s `withCString` pointer dangled by the time libghostty consumed it.
* `266dea7` (PR #51) — Cursor: show in focused view regardless of DECTCEM. Deliberate cmux-style divergence from strict DECTCEM compliance. M2 follow-up; relevant for the gtk4-rs cursor draw in Phase 7 commit 4 (mirrors the same behavior, inline-commented).
* `aebd408` (PR #52) — Tab pills: fix UTF-8 in titles + horizontal scroll on overflow. Two dogfooding bugs combined: OscScanner buffered body bytes as Latin-1 (mangling emoji); NSStackView grew the window per new tab.
* `b5b7838` (PR #53) — Swift OscScanner + KeyEncoder coverage (49 new tests). Regression guard for the M1 UTF-8 lifetime bug + the OSC UTF-8 fix. 8 → 57 tests across the Swift target.
