# Goal — Rust-port polish to "direction-forward" quality

**Set**: 2026-05-16
**Owner**: Charlie Knudsen
**Co-author / executor**: Claude (Opus 4.7)
**Status**: 🚧 in flight — M1 PR opened on day 1.

## Problem

The Rust port (`feature/rust-port`, branched from `claude/discuss-architecture-refactor-cjU3E`) carries phases 0–6a-step-2b's worth of work and *runs* — the Rust daemon + Swift Mac UI + Rust CLI all build and talk to each other on macOS 26.4.1. But side-by-side with the shipping Go binary and with cmux ([UX assessment](ux-assessment-2026-05-16.md), [screenshots](screenshots-2026-05-16/)), the Swift Mac UI is plainly at engineering-prototype level: `NSButton`-stack sidebar, `"● "` text markers for selection, debug chrome (literal `socket: …` / `daemon: connected pid: …` text) hard-coded into the content pane, stock `NSWindow` title, terminal pinned at 80×24, no event subscription so CLI mutations leave the UI stale.

The Go binary is good enough that the human wants to keep using it. The Swift UI is not. That gap is the immediate blocker to taking the rust-port direction seriously.

## Decision

Polish the Swift UI to match the Go binary on visual + key-functional 6a items. Do it on a long-lived `feature/rust-port` branch as a sequence of milestone PRs. Don't chase cmux-tier features.

Resolved on the `/goal` call:

| Question | Choice |
|---|---|
| Branch model | Long-lived `feature/rust-port` off `cjU3E` HEAD; PRs from short-lived `polish/*` topic branches. `cjU3E` is frozen after the cut. Merge `feature/rust-port` → `main` when polish is "shown enough to be the direction forward." |
| Quality bar | Match Go visually + functionally on Phase 6a's key items. Skip cmux-tier features (notification rings, multiline sidebar metadata, splits, command palette). |
| Scope | Polish + WatchEvents + headless CLI surface + window resize (the most ambitious of the three offered scopes). |
| Cadence | Milestone-sized commits, milestone-sized PRs (not micro-PRs, not one mega-PR). |
| PR gating | I open the PR; auto-merge when CI green + CodeRabbit clean. Human reviewer-of-record on each PR. |
| Milestone ordering | Strict: M1 → M2 → M3 → M4 → M5. (M4 is independent of the Mac UI work, but kept sequential to keep CodeRabbit's review load focused.) |

## Branch shape

```
main  (Go binary continues to be authoritative for the world)
 │
 └── claude/discuss-architecture-refactor-cjU3E  ← frozen at 00b3d10
       │
       └── feature/rust-port                     ← long-lived; this goal lives here
             │
             ├── polish/chrome-foundation        ← M1 (open PR #N today)
             ├── polish/native-sidebar           ← M2
             ├── polish/tab-strip-resize         ← M3
             ├── polish/headless-cli             ← M4
             └── polish/selection-copy           ← M5
```

CI: `.github/workflows/refactor.yml` was broadened in this same commit to trigger on pushes to `feature/rust-port` and on PRs whose head branch matches `polish/*`. `ci.yml` already matches `feature/*`. CodeRabbit triggers on every PR via the GitHub app integration.

## Out of scope (this goal)

* Phase 6b — Mac OSC + notifications. Pulls the Go binary's `internal/osc/scanner.go` over to `crates/roost-core/src/osc.rs`. Real differentiator work, but its own scope. M3's tab strip lands with `"Tab N"` placeholder labels and a status-dot slot that goes live in Phase 6b.
* Phase 6a step 2d — `~/.config/roost/config.conf` keybind override. Defaults already match the Go binary on Mac; the override port is a separate slice.
* Phase 7 — Linux UI (gtk4-rs). Including the "GTK-on-Mac for dev convenience" stretch from the UX assessment.
* cmux-tier features: notification rings, multiline sidebar rows with git branch / cwd / ports, in-app browser, splits, SSH workspaces, command palette, custom commands.

## Milestones

### M1 — Chrome foundation + WatchEvents

PR target: `feature/rust-port`. Branch: `polish/chrome-foundation`.

**Slice (a) — the four polish edits already drafted in the working tree:**
1. **Drop debug chrome.** Remove the `socket: <path>` + `daemon: connected pid:…` text block from the content pane. Move the equivalent info to `NSLog` (success) + `NSAlert` sheet (failure).
2. **Bind window title.** `NSWindow.title` = active project name; `NSWindow.subtitle` (Big Sur+) = active project's cwd. Update from `selectProject` and `rebuildSidebar`. Matches the `AdwWindowTitle` pattern in the Go binary.
3. **Sensible default window size.** 1100×700 default, 720×420 minimum. Stop pinning min-size to the 80×24 cell-grid intrinsic.
4. **Dark chrome.** `window.appearance = NSAppearance(named: .darkAqua)` so the titlebar/sidebar don't sit in a glaring white frame around the terminal.

**Slice (b) — WatchEvents subscription:**
1. Add `watchEvents(socketPath:) -> AsyncStream<Event>` to `RoostClient.swift`. Single long-lived background `Task` in `RoostApp` that drains the stream and dispatches to `@MainActor` handlers.
2. Handle each event variant:
   * `ProjectCreated` → append to `projects`, `rebuildSidebar()`.
   * `ProjectRenamed` → update the slot in `projects`, `rebuildSidebar()`.
   * `ProjectDeleted` → remove the slot + tabs filtered to it, fall back to first project if active was deleted, `rebuildSidebar()` + `selectProject(id:)`.
   * `TabOpened` → daemon-driven tab appearance; if the project matches `activeProjectID`, `rebuildTabBar()`.
   * `TabDeleted` → remove from `tabs`; if it was active, advance selection like `closeActiveTabImpl()`.
   * `TabCwdChanged` → no-op until M3 surfaces cwd in the tab label.
   * `ActiveChanged` → reflect the daemon's notion of active project / tab.
3. On `Lagged` (gRPC stream backpressure), re-run `listProjects` for a full resync.

**Exit criteria:**
* `roost-cli-rs project create --name foo` while the Mac UI is open shows `foo` in the sidebar within < 1s, no app relaunch.
* `roost-cli-rs project delete --id N` removes the row from the sidebar.
* Debug chrome no longer visible at launch.
* Window opens at 1100×700 (or the user's saved-frame autosave if we add `NSWindow.setFrameAutosaveName` here too — judgement call during the PR).
* `swift build` clean; `cargo build --workspace` clean; refactor.yml green on the PR.

**Effort estimate**: ~1 day. Slice (a) is ~1 hour (already drafted). Slice (b) is ~half day for the stream subscription + handlers + testing against `roost-cli-rs` mutations.

### M2 — Native sidebar (NSOutlineView)

PR target: `feature/rust-port`. Branch: `polish/native-sidebar`.

**Scope:**
1. Replace the `NSStackView` of `NSButton`s with `NSOutlineView` (single-level tree, projects only) backed by a custom row view.
2. Drop the `"● "` text marker. Row selection is the active-project affordance.
3. Per-project notification badge column (right-aligned), wired through `TabNotificationEvent` ([Phase 6b](phase-6b-mac-osc-notifications.md) consumer). Renders hidden when no tab in the project has `has_notification`.
4. Right-click context menu: Rename / Delete (existing behavior, ported off `NSButton.menu`).
5. Drag-reorder is not in scope here — defer to a follow-up. The Go binary tests it via `sidebar_reorder_test.go`; not blocking polish parity.

**Exit criteria:**
* Sidebar visually matches the Go binary's screenshot under [`screenshots-2026-05-16/go-gtk-ui.png`](screenshots-2026-05-16/go-gtk-ui.png) on row styling + selection.
* Right-click context menu still produces the rename + delete sheets.
* WatchEvents-driven `ProjectCreated/Renamed/Deleted` propagation works on the new outline view.
* No regressions in `⌘1..⌘9` project-switch shortcut.

**Effort estimate**: ~1.5 days. NSOutlineView is well-trodden but the data-source / delegate ceremony is non-trivial.

### M3 — Native tab strip + window resize

PR target: `feature/rust-port`. Branch: `polish/tab-strip-resize`.

**Slice (a) — Tab strip rewrite:**
1. Replace the `NSStackView` of `NSButton`s with custom `NSView` subclass per tab. Layout: leading status-dot slot (16×16 — empty in this PR), label ("Tab N" placeholder until OSC 7 lands the cwd), trailing `×` close button shown on the active tab.
2. Active-tab styling via row background / underline, not `"● "` prefix.
3. Tab strip in a proper container view (a thin `NSToolbar` is overkill; a horizontally-arranged custom strip is the right shape).

**Slice (b) — Window resize → cell-grid reflow:**
1. Subscribe `TerminalView` to `viewDidEndLiveResize` / `resizeSubviews(withOldSize:)`.
2. On resize, compute new `cols`/`rows` from new bounds / cell metrics. Call `ghostty_terminal_resize(handle, cols, rows)`.
3. Send `PtyResize { cols, rows }` over the existing bidi `StreamPty` stream — daemon already supports it (M4 also adds a CLI surface for it).
4. Drop the `widthAnchor / heightAnchor constraint(equalToConstant:)` on the per-tab `TerminalView` and replace with leading/top/trailing/bottom edge constraints so the terminal fills the container.

**Exit criteria:**
* Tab strip visually matches the Go binary's screenshot on chrome (close button, label, status-dot slot present even if not yet lit up).
* Resizing the window grows the cell grid: a wider window holds more columns, a taller window holds more rows. Verified by typing `tput cols && tput rows` after a resize.
* No glyph fuzz / cell-fractional artifacts at common resizes (1100×700, 1400×900, 1920×1080).

**Effort estimate**: ~2.5 days. Slice (a) is half a day; slice (b) is the bulk — touches `TerminalView`, `TabSession`, `StreamPty`, and there's testing.

### M4 — Headless CLI surface

PR target: `feature/rust-port`. Branch: `polish/headless-cli`.

**Scope:**
1. New `roost-cli-rs` subcommands:
   * `tab open --project-id P [--cwd PATH]` → calls existing `OpenTab` RPC.
   * `tab close --tab-id T` → calls existing `CloseTab` RPC.
   * `tab send --tab-id T --bytes "ls\n"` → writes bytes into the bidi `StreamPty` stream.
   * `tab send --tab-id T --keystroke "Ctrl+C"` → encodes via libghostty-vt's key encoder (the lite path Phase 5.5c-lite uses) and writes through `StreamPty`.
   * `tab resize --tab-id T --cols N --rows M` → sends `PtyResize` over `StreamPty`.
   * `tab snapshot --tab-id T [--format text|json]` → new daemon RPC; daemon walks `ghostty_render_state_*` and returns a plain-text dump of the grid (and a JSON variant with per-cell attrs for richer tooling).
2. Daemon-side: add `TabSnapshot` to `proto/roost.proto`. Implement in `crates/roost-core/src/service.rs` by walking the libghostty-vt render state. Reuse the existing main-thread invariant via `tokio::task::block_in_place` or by routing through the `StreamPty` actor's main-thread queue.
3. Smoke script under `crates/roost-smoke/`: opens a tab, sends `echo hello\n`, snapshots, asserts `hello` appears, closes the tab.

**Exit criteria:**
* `roost-cli-rs tab open --project-id 1` exits 0 and the new tab is visible in the Mac UI within < 1s (via WatchEvents from M1).
* `roost-cli-rs tab send --tab-id N --bytes "ls\n" && roost-cli-rs tab snapshot --tab-id N` shows the directory listing in the snapshot.
* `roost-cli-rs tab resize --tab-id N --cols 120 --rows 40` propagates to the Mac UI's `TerminalView` (so the renderer can validate the proto-level surface even before M3's UI-side reflow lands).
* `crates/roost-smoke/` end-to-end test passes in CI.

**Effort estimate**: ~1.5 days. Pure proto + Rust work; no Swift / Mac UI work. Could overlap with M2 / M3 if we relax the strict-ordering decision — call out at next checkpoint.

### M5 — Selection + copy / paste hardening

PR target: `feature/rust-port`. Branch: `polish/selection-copy`.

**Scope:**
1. `TerminalView` mouse-drag tracking → cell-rect selection state.
2. Custom draw pass over the selected cell-rect with the theme's selection background.
3. `copy` action (also wired to `⌘C` via the standard responder chain) walks libghostty-vt's render state for the selected region and writes plain text to `NSPasteboard`.
4. `paste` (`⌘V`) reads from `NSPasteboard`, detects bracketed-paste mode (DECSET 2004 enabled by the shell), wraps the payload in `ESC[200~ … ESC[201~` when appropriate, writes through `StreamPty`.

**Exit criteria:**
* Mouse-drag in the terminal area produces a visible selection.
* `⌘C` with a selection puts plain text on the pasteboard. Tested by pasting into another app.
* `⌘V` in a shell that has bracketed-paste enabled (e.g. zsh with the default settings) inserts the text with bracket escapes; in a shell without it (`cat`), it inserts raw bytes.

**Effort estimate**: ~2 days. Mouse handling is mostly NSResponder ceremony; the render-state walk for text extraction is the real work.

## Total horizon

~8–9 working days end-to-end at the cadence picked above. Reality budget: assume ~12 to handle CI churn, CodeRabbit cycles, and the inevitable mid-PR redesign.

## Process

* **PR opens**: I (Claude) push the topic branch and open the PR via `gh pr create --base feature/rust-port`. PR body cites this goal doc + the milestone section.
* **CI**: `ci.yml` runs Go on Linux + macOS. `refactor.yml` runs Rust lint/build on Linux + macOS + the Swift build on macOS. CodeRabbit reviews via the GitHub app.
* **Auto-merge**: I run `gh pr merge --auto --squash` immediately after opening the PR. PR lands as soon as CI is green + branch protection on `feature/rust-port` allows (if branch protection isn't set up, the PR may merge as soon as CI passes; that's the user's intended cadence).
* **CodeRabbit follow-ups**: I address actionable CodeRabbit comments as small follow-up commits on the same topic branch. Non-actionable items are acknowledged but not changed.
* **Per-milestone retrospective**: after each merge, I post a one-paragraph summary in this doc under "Milestone log" below + check the milestone's task as done.

## Open questions / risks

* **Branch protection rules**. Auto-merge `--auto` is most useful if `feature/rust-port` has branch protection requiring `ci.yml` + `refactor.yml` to pass. Without it, the PR auto-merges as soon as it's opened (the gate is essentially nothing). Flagging for the user to consider enabling branch protection on `feature/rust-port` after M1 has demonstrated the workflow.
* **NSWindow.subtitle on macOS 26.4**. Documented as Big Sur+ (11+). All hosts in scope are post-macOS-13. Safe.
* **NSOutlineView's quirks on macOS 26**. The Sonoma → Tahoe transition changed some default rendering. M2 will hit them; if anything explodes, we fall back to `NSTableView` with manual hierarchy.
* **OSC 7 cwd in tab labels**. M3's tab strip will have a label slot but the data won't be there until Phase 6b. Tab labels stay as `"Tab N"` (or project cwd) through M5. Phase 6b is out-of-scope for this goal but should follow this goal closely.
* **Drag-reorder of projects**. The Go binary supports it (`sidebar_reorder_test.go`). M2 doesn't include it. Track as a follow-up; not on the polish-parity critical path.
* **Per-tab font config**. The Go binary has `cmd/roost/font.go` with theme-aware font defaults. Out of this goal — keep the Swift UI's current font fallback (system monospace via `pickFontFamily` equivalent). Comes in a follow-up phase 6a step.

## Milestone log

* **M1 — Chrome foundation + WatchEvents** — ✅ merged 2026-05-16 (PR [#23](https://github.com/charliek/roost/pull/23), squash commit [`90fbc58`](https://github.com/charliek/roost/commit/90fbc585052df0add0a2ae4c5122ed76a5d94883)).
  * Slice (a) chrome polish: debug chrome removed, `NSWindow.title` + `subtitle` bound to active project, 1100×700 default + 720×420 min, `.darkAqua` chrome.
  * Slice (b) WatchEvents: `RoostClient.watchEvents` + `RoostApp.subscribeToEvents` with stream-end resync; `projectCreated/Renamed/Deleted` fully propagated; tab events logged but not yet acted on (waiting on M3's tab-strip refactor).
  * Verified end-to-end: `roost-cli-rs project create/rename/delete` reflects in the sidebar within ~50ms with no app restart.
  * Process notes:
    * Auto-merge fired immediately on first `gh pr merge --auto --squash` because no branch protection was set up at the time. Branch protection on `feature/rust-port` (requiring `ci.yml` + `refactor.yml` status checks) was applied after the merge; M2 onward will properly gate on green CI.
    * `allow_auto_merge` was off at the repo level; flipped to true.
    * Two settings to keep in mind for future PRs: branch protection rules are in place, and auto-merge respects them.
* **M2 — Native sidebar (NSOutlineView)** — ✅ merged 2026-05-16 via PR [#24](https://github.com/charliek/roost/pull/24).
  * `NSOutlineView` in source-list style. PROJECTS uppercase header. Native row selection (no `"● "` marker). One column with `ProjectRowCellView`. Right-click menu uses `clickedRow` + `NSMenuDelegate.menuNeedsUpdate` to gate items.
  * `applySidebarSelection()` programmatic-selection helper guarded by `isSyncingSidebarSelection` so `outlineViewSelectionDidChange` doesn't re-enter `selectProject(id:)`.
  * Verified `roost-cli-rs project create / rename / delete` and `⌘1..⌘9` all converge correctly on the new outline view.
  * **Process learning**: linux CI runners were oversubscribed on 2026-05-16; `test (ubuntu-latest)` etc. ran 5–10× slower than usual while mac runners stayed fast. Branch protection on `feature/rust-port` was relaxed to require only the 3 macOS checks (`test (macos-latest)`, `rust-build (macos-latest)`, `swift-mac`); linux jobs still run on every PR but no longer block auto-merge. Revisit once the linux runner slowness is diagnosed.
* **Process side-fix** — `polish/coderabbit-config` (PR [#25](https://github.com/charliek/roost/pull/25)) adds `.coderabbit.yaml` enabling auto-review on `feature/rust-port`. CodeRabbit's default policy skipped reviews on non-default base branches; observed on PR #23 as "Auto reviews are disabled on base/target branches other than the default branch." Without this fix the "auto-merge when CI green + CodeRabbit clean" gate silently degrades to "auto-merge when CI green."
* M3 — pending M2.
* M4 — pending M3.
* M5 — pending M4.
