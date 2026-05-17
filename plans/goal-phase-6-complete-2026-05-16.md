# Goal — Finish Phase 6 (Phase 6b OSC/notifications + the three M-followup slices)

**Set**: 2026-05-16
**Owner**: Charlie Knudsen
**Co-author / executor**: Claude (Opus 4.7)
**Status**: ✅ closed — all P1–P9 merged on `feature/rust-port` (2026-05-16)
**Predecessor**: [`goal-rust-port-polish-2026-05-16.md`](goal-rust-port-polish-2026-05-16.md) (M1–M8, all merged on `feature/rust-port`)
**Companion**: Phase 7 (full Linux UI on top of M8's gtk4-rs spike) — being driven separately on the user's Linux laptop, NOT part of this goal.

## Problem

The rust-port polish goal closed M1–M8 cleanly. What's left to finish Phase 6 — the named follow-ups from the predecessor goal's closure section, in their original order:

1. **`polish/keybind-config`** — the larger half of M6. Port `cmd/roost/shortcuts.go` + `shortcuts_test.go` to Swift; install the resolved bindings into the NSMenu so users with existing `~/.config/roost/config.conf` `keybind = …` lines get their overrides honored on the Swift binary.
2. **`polish/font-zoom`** — M6 cleanup. `font_increase` / `font_decrease` / `font_reset` shortcuts (`⌘+` / `⌘-` / `⌘0`) with `TerminalView` cell-grid + PtyResize reflow on font change.
3. **`polish/palette`** — M6 cleanup. Per-cell palette overrides via `ghostty_terminal_set` so the active theme's `palette[0..256]` flows into libghostty's SGR-color lookup. M6 only switched canvas / selection colors; `ls --color`, `git diff` etc. still use libghostty's compiled-in palette.
4. **Phase 6b (Mac OSC + notifications)** — the actual differentiator. Daemon-side OSC scanner port → daemon routing for OSC 0/1/2 (title), 7 (cwd), 9/99/777 (notification) → Mac UI consumption → per-tab + sidebar badges → desktop notifications via `UNUserNotificationCenter` → Claude Code hook end-to-end. M3's tab-strip status-dot slot and M2's sidebar badge column were both reserved for this work.

Wide-char rendering, secondary shortcuts (`cycle_tab_*`, `toggle_sidebar`, `rename_tab`), and `tab snapshot` are deliberately out of scope for this goal — they belong on follow-up branches.

Phase 7 (full Linux UI on top of the M8 spike) is the user's own work on their Linux laptop, not part of this goal.

## Decision

Close the four items above on `feature/rust-port` through milestone-sized PRs, same process as the predecessor goal (long-lived `feature/rust-port`, short-lived `polish/*` topic branches, auto-merge gated on the 3 macOS CI checks).

| Question | Choice |
|---|---|
| Branch model | Same as predecessor: `polish/*` topic branches → `feature/rust-port`. |
| CI gates | Same: macOS-only required checks (`test (macos-latest)` + `rust-build (macos-latest)` + `swift-mac`). Linux jobs informational. |
| Ordering | Two parallel tracks — see below. |
| Cadence | Milestone-sized PRs, larger than micro but smaller than mega. |
| CodeRabbit | Auto-review enabled via `.coderabbit.yaml` (landed pre-M5). |

### Two parallel tracks

The Phase 6b OSC chain is sequential by nature (scanner → routing → UI consumption → notification surfaces). The three M-followup slices are independent of each other AND of the OSC chain.

* **Track A (Phase 6b OSC + notifications)** — P4 → P5 → P6 → P7 → P8 → P9. Strict order; each milestone consumes the previous.
* **Track B (M-followup cleanup)** — P1, P2, P3. Independent of A and of each other; land in any order. Good filler during Track A CI / review wait time.

Track A milestones modify the daemon's OSC routing layer; two simultaneous Track A PRs would conflict. Track B PRs touch independent surfaces (keybind config, font handling, palette FFI) and can run in parallel with each other AND with any Track A milestone.

## Branch shape

```
main  (Go binary, authoritative for now)
 │
 └── feature/rust-port  (current HEAD: 2aaa398, M1–M8 + closure commit)
       │
       ├── polish/keybind-config          (P1 — track B)
       ├── polish/font-zoom               (P2 — track B)
       ├── polish/palette                 (P3 — track B)
       ├── polish/osc-scanner-port        (P4 — track A)
       ├── polish/osc-daemon-routing      (P5 — track A)
       ├── polish/ui-osc-detect-report    (P6 — track A)
       ├── polish/tab-notification-badges (P7 — track A)
       ├── polish/desktop-notifications   (P8 — track A)
       └── polish/claude-hook-flow        (P9 — track A)
```

When all 9 land, Phase 6 is closed on `feature/rust-port` and the branch is ready to merge to `main` (modulo Phase 7's full Linux UI, which is the user's parallel work).

## Out of scope (this goal — see followups/ at end)

* Wide-char (CJK + emoji) cell width handling — Phase 6a step 2h.
* Secondary shortcuts other than `font_*`: `cycle_tab_prev/next`, `toggle_sidebar`, `rename_tab` — Phase 6a step 2e.
* `tab snapshot` RPC — M4 deferred slice.
* Phase 7 — full Linux UI (Cairo + Pango cell renderer + sidebar + tab bar + StreamPty round-trip in `crates/roost-linux`). The user's parallel work on their Linux laptop.
* Phase 8 — code-sign + notarize + DMG + Sparkle for the M7 `.app` bundle.
* Phase 9 — cutover (delete `cmd/` + `internal/`). Eyes-open destructive PR after this goal closes.
* cmux-tier features: notification rings, multiline sidebar metadata rows, in-app browser, splits, SSH workspaces, command palette.

---

## Milestones

### Track B (independent M-followups)

#### P1 — Keybind override config (`~/.config/roost/config.conf`)

PR target: `feature/rust-port`. Branch: `polish/keybind-config`.

**Scope:**
1. Port `cmd/roost/shortcuts.go` (140 LOC) + `cmd/roost/shortcuts_test.go` (316 LOC) to Swift. Reuse the action-name namespace verbatim: `new_tab`, `close_tab`, `new_project`, `rename_project`, `rename_tab`, `cycle_tab_prev`, `cycle_tab_next`, `paste`, `copy`, `font_increase`, `font_decrease`, `font_reset`, `toggle_sidebar`, `switch_project_1..9`, `switch_tab_1..9`, `unbind`. The Mac UI today wires actions to specific NSMenu items directly; this PR rewires through a `KeybindTable` resolved from config.
2. Extend M6's `Config.swift` to parse `keybind = <trigger> = <action>` lines. Trigger parser (`triggerToAccel` in Go) handles modifier aliases — `super`/`cmd`/`command`, `ctrl`/`control`, `alt`/`option`/`opt`. Port the parser + its tests verbatim.
3. `canonicalizeBindings`: a user's `keybind = cmd+t = unbind` correctly removes the `super+t` default. Port the dedup / unbind logic.
4. Install the resolved bindings into the NSMenu at startup: each `NSMenuItem.keyEquivalent` + `keyEquivalentModifierMask` driven by the resolved action map.

**Exit criteria:**
* `keybind = cmd+t = new_tab` in the config file overrides the default and is reflected in the NSMenu's File menu shortcut listing.
* `keybind = cmd+t = unbind` removes `⌘T` from the File menu.
* Modifier alias parsing matches the Go binary's tests case-for-case.
* No regression on the default-binding behaviour for users with no config file.

**Effort estimate**: ~2 days. Trigger parser is the bulk; action dispatch infrastructure already exists.

**Ordering note**: P2 + P8 + P9 ideally land after P1 so their action handlers route through the new keybind table. If parallelism matters they can land first with direct-selector wiring + this PR retargets the actions.

#### P2 — Font zoom shortcuts (`font_increase` / `font_decrease` / `font_reset`)

PR target: `feature/rust-port`. Branch: `polish/font-zoom`.

**Scope:**
1. Three new actions on the `RoostApp` action surface:
   * `font_increase` — bumps `activeFont` size by 1pt (clamped at e.g. 32pt max).
   * `font_decrease` — bumps down by 1pt (clamped at 8pt min).
   * `font_reset` — restores `config.fontSize ?? 14`.
2. On change, rebuild `TerminalView.cellSize` from the new font + trigger a re-layout. M3's `setFrameSize` path naturally picks up the new cell metrics: new cols / rows are floor-quantized, `ghostty_terminal_resize` fires, `PtyResize` propagates through `StreamPty` to the daemon, the shell sees the new `stty size`. The font change ALSO has to bubble into existing TerminalViews held by other TabSessions, not just the active one.
3. Default bindings: `⌘+` / `⌘-` / `⌘0` per the Go binary's `cmd/roost/keymap.go` defaults. Wired through P1's keybind table once that lands, OR direct selectors as a stopgap.

**Exit criteria:**
* `⌘+` enlarges the cell grid; `stty size` in the tab shows fewer columns and rows.
* `⌘-` shrinks; `⌘0` restores the config default.
* Switching tabs preserves each tab's current zoom level (per-tab vs global — pick "global" to match Go; document the choice in the PR).
* No regression on M3's resize / reflow path.

**Effort estimate**: ~1 day. Font change → cell metric recompute → reflow. The "global vs per-tab" decision is the only design choice.

#### P3 — Per-cell palette override

PR target: `feature/rust-port`. Branch: `polish/palette`.

**Scope:**
1. After M6's `Theme.loadBundled`, push the theme's `palette[0..256]` into libghostty-vt for each `TerminalView` via `ghostty_terminal_set` with the appropriate palette-index option key. Confirm signature against `third_party/ghostty/out/include/ghostty/vt/terminal.h` — the Ghostty C API exposes per-color setters.
2. Mirror what the Go binary does in `cmd/roost/theme.go::applyToTerminal` (the function that walks the theme and pushes each palette entry into the cgo-wrapped terminal). Side effect: cells that use SGR colors (`ls --color=always`, `git diff`'s reds/greens, `htop`'s palette) flip to the theme's palette instead of libghostty's compiled-in default.
3. Cell fg / bg colors that defer to the libghostty default (no SGR override on the cell) already pick up M6's canvas color — this PR is strictly about the 16/256 palette indices.

**Exit criteria:**
* `ls --color=always /usr` in a Roost tab uses the active theme's palette colors. Catppuccin Mocha mauve, Dracula's purple-pink, etc.
* `git diff HEAD~..HEAD` renders adds / deletes in the theme's red + green.
* No regression on M6's canvas / foreground / selection colors.
* Theme switch (after restart, since M6 doesn't hot-reload) applies the new palette.

**Effort estimate**: ~0.5 day. FFI call per palette entry; one place to add it (`TerminalView.init` after `ghostty_terminal_new`).

---

### Track A (Phase 6b OSC + notifications — sequential)

#### P4 — OSC scanner port (daemon-side, Rust)

PR target: `feature/rust-port`. Branch: `polish/osc-scanner-port`.

**Scope:**
1. Port `internal/osc/scanner.go` (301 LOC) + `internal/osc/scanner_test.go` (359 LOC) to a new `crates/roost-core/src/osc.rs`. The Go scanner is a stateful byte-by-byte state machine that emits `OSCEvent { command, payload }` once a sequence completes (BEL or ST terminator). Covers OSC 0/1/2/7/9/99/777.
2. Bring the Go test fixtures across verbatim — they're the spec for the edge cases (empty payloads, embedded ST inside strings, BEL-vs-ST handling, OSC 777 sub-commands).
3. **Implementation choice — option A vs B**:
   * **A.** Hand-port the Go state machine to safe Rust. Clean ownership, no new FFI surface, matches what the daemon will eventually use cross-platform. ~300 LOC.
   * **B.** Wrap libghostty-vt's already-exposed `ghostty_osc_new` / `ghostty_osc_next` / `ghostty_osc_command_type` / `ghostty_osc_command_data` via `crates/roost-vt`. Less code, one upstream-tracking dep, but adds a daemon dependency on libghostty-vt that doesn't exist today.
   * **Recommend option A** — keeps the daemon libghostty-free, port is small, test fixtures travel with it. Document the choice in the PR.

**Exit criteria:**
* `crates/roost-core/src/osc.rs` exists with `OscScanner` + `OscEvent` types.
* Every test case from `internal/osc/scanner_test.go` is mirrored under `#[cfg(test)]` in `osc.rs` and passes.
* Public surface limited to what P5 will consume — `feed(&mut self, bytes: &[u8]) -> impl Iterator<Item = OscEvent>` or similar.
* `cargo test --workspace --exclude roost-linux` green on the required macOS gate.

**Effort estimate**: ~1.5 days. Port is mechanical; the time goes to porting test cases and making the Rust idiom feel native.

#### P5 — Daemon OSC routing

PR target: `feature/rust-port`. Branch: `polish/osc-daemon-routing`.

**Scope:**
1. Wire the P4 scanner into the daemon's existing `ReportOsc` handler (currently a thin stub in `crates/roost-core/src/service.rs`). The UI sends raw OSC bytes via `ReportOsc`; the daemon scans + dispatches.
2. Dispatch table per OSC command:
   * **OSC 0/1/2** (title) → `Workspace::set_tab_title(id, title, user=false)`. If `tab.user_titled`, drop (locked tabs ignore inbound title changes — mirrors Go).
   * **OSC 7** (cwd) → percent-decode the `file://host/<path>` form → `Workspace::set_tab_cwd(id, cwd)` → emit `TabCwdChangedEvent`. (`crates/roost-core/src/service.rs::parse_cwd_from_osc7` already has scaffolding; reuse.)
   * **OSC 9** (notification, title-only) and **OSC 777** (`notify;title;body`) → `Workspace::fire_notification(id, title, body)` (emits `NotificationEvent` + sets `tab.has_notification=true` + emits `TabNotificationEvent`) UNLESS `tab.hook_active` is true, in which case suppress (the Claude Code hook owns the notification surface during its window — mirrors Go).
   * **OSC 99** — id'd notification with "dismiss + replace by id" semantics. Match Go's behaviour.
3. Tests for the suppression-under-hook-active rule: the most subtle part of the Go scanner (DL-8 in vision.md flagged it). Port `cmd/roost/notify_test.go`'s hook-suppression cases.

**Exit criteria:**
* `roost-cli-rs notify --tab N --title T --body B` and `ReportOsc` over the wire both flow through the same `Workspace::fire_notification` path.
* `SetHookActive(tab, true)` followed by OSC 9/777 from that tab DROPS rather than emitting `NotificationEvent`. Test asserts the drop.
* `TabCwdChangedEvent` propagates over `WatchEvents` for OSC 7. The Mac UI's `handleEvent` (M1's WatchEvents subscription) is the consumer in P6.
* `TabNotificationEvent` propagates over `WatchEvents` for unsuppressed OSC 9/777.

**Effort estimate**: ~1.5 days. Dispatch is small; the test coverage for the hook-suppression edges dominates.

#### P6 — Mac UI OSC detection + ReportOsc upcall + lit-up status dot / tab cwd

PR target: `feature/rust-port`. Branch: `polish/ui-osc-detect-report`.

**Scope:**
1. Harvest OSC events from libghostty-vt during the `TerminalView` walk. There's a `ghostty_terminal_*` API for this — confirm signature first; may require a `roost-vt` bindgen extension if not already exposed. The Go binary's equivalent intercepts the OSC bytes before passing them to libghostty-vt's VT writer; pick whichever shape matches the libghostty FFI more naturally from Swift.
2. For each detected OSC, call `ReportOsc(tab_id, raw_bytes)` over the existing gRPC client.
3. Extend `RoostApp.handleEvent(_:)` (the M1 WatchEvents handler) to consume the events P5 emits:
   * `TabTitleChangedEvent` → update the tab pill's label (instead of `"Tab N"`).
   * `TabCwdChangedEvent` → update the tab pill's label to a tilde-abbreviated cwd, matching the Go binary's `cmd/roost/app.go::formatTabTitle`. M3's tab strip already has the label slot — this fills it.
   * `TabStateChangedEvent` → update the tab pill's status-dot color (running=green, idle=gray, needs-input=yellow, error=red). M3's tab strip reserved the 10×10 dot slot — this lights it up.

**Exit criteria:**
* A shell that writes `printf '\\e]0;new title\\a'` reflects in the Mac tab pill within one PTY round-trip.
* A shell that `cd`s to a new dir (zsh OSC 7 emits automatically) reflects in the pill's cwd label.
* `roost-cli-rs notify --tab N --title T --body B` lights up the tab's status dot.
* No regression on M3's tab strip behaviour (clicks, close button, active-tint).

**Effort estimate**: ~2 days. FFI investigation is the unknown; UI plumbing is mechanical.

#### P7 — Per-tab + per-project notification badges (sidebar + tab strip)

PR target: `feature/rust-port`. Branch: `polish/tab-notification-badges`.

**Scope:**
1. Polish pass on the badge slots P6 lit up:
   * Tab pill: replace the gray placeholder dot with a small filled circle in the theme's accent color. Pulse / highlight on first arrival, steady-state after.
   * Sidebar row (M2's `ProjectRowCellView`): small badge view in the right column (M2 reserved it). Theme-tinted dot or `(N)` count of notified tabs in the project. Consume `TabNotificationEvent` and aggregate per-project.
2. `ClearTabNotification` on tab focus: when the user clicks a notified tab (or it becomes active via `⌘1..⌘9` / arrow keys), the daemon's `ClearTabNotification` RPC fires; the badge clears across all clients via `TabNotificationEvent { has_pending: false }`.
3. New shortcut `jump_to_unread` (default `⌘⇧U`, override-able via P1): jumps to the next unread tab (most recent `has_notification=true` tab in the active project; fall back to the first project with any notified tab if none in active). Mirrors cmux's "jump to latest unread."

**Exit criteria:**
* Visual: badges visible side-by-side with the Go binary on the same notification scenario.
* Functional: tab focus clears the badge daemon-side and on all watching clients.
* Functional: `⌘⇧U` navigates to the next unread tab; no-op if none.

**Effort estimate**: ~1 day. Renders + one shortcut handler + the focus-clears-badge wiring.

#### P8 — Desktop notifications via UNUserNotificationCenter

PR target: `feature/rust-port`. Branch: `polish/desktop-notifications`.

**Scope:**
1. Subscribe to `NotificationEvent` on the WatchEvents stream (M1's `subscribeToEvents` is the consumer point).
2. For each event, fire a `UNUserNotificationCenter` request with title + body + a `category` of `"roost-tab"` carrying the tab id in `userInfo`.
3. Request authorization on first run via the standard Mac prompt (`UNUserNotificationCenter.requestAuthorization(options: [.alert, .sound])`). Trigger the prompt on `applicationDidFinishLaunching` so it arrives at a predictable moment, not mid-session when the first notification would naturally fire.
4. Notification-click handler: when the user clicks the banner, raise the Roost window, switch to the project that contains the tab, focus the tab. Reuses M2's `selectProject` + M3's `selectTab` paths.

**Exit criteria:**
* `roost-cli-rs notify --tab N --title T --body B` produces an actual macOS notification banner (after the first-run permission grant).
* Click on the banner brings Roost forward + focuses the originating tab.
* No banners when the originating tab is `hook_active` — P5's suppression flows through.

**Effort estimate**: ~1 day. UNUserNotificationCenter ceremony is well-trodden; the click handler is the only piece that needs care.

#### P9 — Claude Code hook end-to-end (parity with Go)

PR target: `feature/rust-port`. Branch: `polish/claude-hook-flow`.

**Scope:**
1. Port `cmd/roost-cli/claude_hook.go` (the `claude-hook EVENT` subcommand that the user's `~/.claude/hooks/<event>.sh` script invokes) to `roost-cli-rs`. The hook script's contract: when an event fires, call `roost-cli claude-hook <EVENT>` with the JSON-encoded event on stdin; the CLI sends `SetHookActive(tab, true)` on entry, fires a `Notify` on relevant events, sends `SetHookActive(tab, false)` on exit.
2. Port `cmd/roost-cli/claude_install.go` (the `claude install [--force]` subcommand that drops the per-event hook scripts into `~/.claude/hooks/`). Confirm script paths + env vars (`ROOST_TAB_ID` etc.) match the Go binary's so existing Claude installations Just Work after a `roost-cli-rs` upgrade.
3. End-to-end test: a sample Claude Code session with the hooks installed should produce identical notification surface behaviour on both binaries (Go `./roost` vs the Swift `Roost.app` from M7).

**Exit criteria:**
* `roost-cli-rs claude install` drops the hook scripts. Verified by `ls ~/.claude/hooks/`.
* A Claude Code session inside a Roost tab fires `set_hook_active(true)` on session start, fires `Notify` on prompt-needs-input events, fires `set_hook_active(false)` on session end. Verified by tailing the daemon's WatchEvents stream.
* Notifications during `hook_active=true` are suppressed at the daemon's OSC layer (P5 already covers this); the hook script owns the surface.

**Effort estimate**: ~1.5 days. Mostly porting; the Claude hook spec is documented in the Go test fixtures.

---

## Total horizon

* Track B: ~3.5 days (P1 2 + P2 1 + P3 0.5).
* Track A: ~8.5 days (P4 1.5 + P5 1.5 + P6 2 + P7 1 + P8 1 + P9 1.5).

End-to-end if serial: ~12 days. With Track B interleaved during Track A's CI / review wait time: realistically ~9 days. Reality budget: assume ~12 to handle CI churn (the Linux runner slowness pattern from the predecessor goal hasn't resolved), CodeRabbit cycles, and inevitable mid-PR redesign.

## Process

* **PR opens**: Claude (or the human) pushes the topic branch and opens the PR via `gh pr create --base feature/rust-port`. PR body cites this goal doc + the milestone section.
* **CI**: Same gate as the predecessor goal — required: `test (macos-latest)`, `rust-build (macos-latest)`, `swift-mac`. Linux jobs run informationally. `gtk-build` matrix continues to fire on `polish/*` PRs.
* **Auto-merge**: `gh pr merge --auto --squash` immediately after opening. Branch protection blocks until the 3 required gates pass.
* **CodeRabbit**: auto-reviews on `feature/rust-port` PRs via `.coderabbit.yaml`.
* **Per-milestone log**: append a one-paragraph summary to the [Milestone log](#milestone-log) below as each PR merges.

## Open questions / risks

* **OSC scanner port choice (option A vs B in P4)**. Recommendation noted; the final call gets made at PR-open time.
* **libghostty-vt OSC surface from Swift (P6)**. Unknown shape until P6 starts. If libghostty doesn't expose detected OSCs in a Swift-friendly way, the Mac UI may need to scan the PTY byte stream itself before feeding libghostty — same workaround the Go binary uses. Doesn't change end-to-end behaviour.
* **`UNUserNotificationCenter` permission UX (P8)**. First-launch experience: triggering a no-op `requestAuthorization` on `applicationDidFinishLaunching` means the prompt arrives at a predictable moment but might annoy users who never plan to grant notifications. Alternative: prompt on the first `Notify` fire. P8 PR-open decision.
* **Hook script path on macOS (P9)**. The Go binary writes hooks to `~/.claude/hooks/` (per `cmd/roost-cli/claude_install.go`). Confirm Claude Code on macOS still reads from that path and that the script invocations + env vars (`ROOST_TAB_ID`, etc.) haven't shifted upstream.
* **Per-tab vs global font zoom (P2)**. Go binary uses global (one zoom level for all tabs). Mirror unless dogfooding shows per-tab is what people want.

## Out-of-goal followups

Captured here so they don't get lost; not part of THIS goal's scope:

* **`polish/wide-char-width`** — Phase 6a step 2h. CJK + emoji cell width via libghostty-vt's per-cell width field. ~0.75 day.
* **`polish/secondary-shortcuts`** — `cycle_tab_prev` / `cycle_tab_next`, `toggle_sidebar`, `rename_tab`. Phase 6a step 2e minus the `font_*` items (which P2 of this goal covers). ~1 day.
* **`polish/tab-snapshot`** — M4-deferred RPC; daemon-side libghostty-vt parse OR a PTY-output ring buffer + new `TabSnapshot { tab_id, format }` proto. Unblocks unattended UI testing. ~1.5 days.
* **Phase 7** — full Linux UI (Cairo + Pango cell renderer + sidebar + tab bar + StreamPty round-trip in `crates/roost-linux`). User's parallel work on their Linux laptop.
* **Phase 8** — code-sign + notarize + DMG + Sparkle for the M7 `.app`.
* **Phase 9** — cutover (delete `cmd/` + `internal/`).

## Milestone log

All nine P-commits landed on `feature/rust-port` 2026-05-16 via stacked `polish/*` PRs auto-merged through branch-protection on the macOS-only required check set.

* P1 — ✅ merged via #34 (`polish/keybind-config`, squash `bd2609a`).
* P2 — ✅ merged via #33 (`polish/font-zoom`, squash `17af7d7`). Landed before P1 — branches were independent.
* P3 — ✅ merged via #32 (`polish/palette`, squash `38fa60e`).
* P4 — ✅ merged via #35 (`polish/osc-scanner-port`, squash `ab20cf3`). Needed a rustfmt amend on the rebased commit (4-line cleanup in `osc.rs`).
* P5 — ✅ merged via #36 (`polish/osc-daemon-routing`, squash `72f3423`). Also needed a rustfmt amend (`if let Err` line break in `service.rs`).
* P6 — ✅ merged via #37 (`polish/ui-osc-detect-report`, squash `2f64da0`).
* P7 — ✅ merged via #38 (`polish/tab-notification-badges`, squash `15863c9`).
* P8 — ✅ merged via #39 (`polish/desktop-notifications`, squash `886c9c1`).
* P9 — ✅ merged via #41 (`polish/claude-hook-flow`, squash `91ad2f1`). Original PR #40 self-merged into the P8 branch when its base lacked required checks; split into a fresh PR (#41) properly stacked.

### Notes from the cascade

* **Stacked-PR retarget cycle**: as each PR landed, the downstream branches were rebased onto the new `feature/rust-port` HEAD (`git rebase --onto origin/feature/rust-port <prev-tip>`), force-pushed, retargeted via `gh pr edit N --base feature/rust-port`, closed + reopened to retrigger CI, and re-armed with `gh pr merge N --auto --squash`. Six iterations of that loop in total.
* **Auto-merge on non-protected bases is instant**: setting `--auto --squash` on a PR whose base branch has no required checks merges immediately if no checks fail synchronously. This bit us on #40 — the P9 commit landed in the P8 branch before there was a chance to retarget. Fixed by force-resetting `polish/desktop-notifications` back to just P8 and opening a fresh PR (#41) for P9.
* **Latent rustfmt drift**: `cargo fmt --all -- --check` failed on the rebased P4 and P5 commits even though the original (pre-rebase) commits had passed CI. Both were trivial style cleanups in newly-introduced sections of `osc.rs` / `service.rs`; amended into the offending commit each time. Possible CI cache or rustfmt environmental difference; not worth chasing further.
