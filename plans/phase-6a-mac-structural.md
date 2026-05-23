# Phase 6a: Mac structural parity

**Status**: ✅ done — closed on `feature/rust-port` via the M1–M7 polish goal ([`goal-rust-port-polish-2026-05-16.md`](goal-rust-port-polish-2026-05-16.md)) and the P1–P3 followups from [`goal-phase-6-complete-2026-05-16.md`](goal-phase-6-complete-2026-05-16.md). Wide-char width (Step 2h) and the `toggle_sidebar`/`rename_tab` secondary shortcuts are deferred to dedicated `polish/wide-char-width` and `polish/secondary-shortcuts` followups noted in the goal doc.

**Exit criteria**:
* Multi-tab support — open / close / switch tabs within a project. ✅
* Project sidebar — list projects, create / rename / delete (`NSOutlineView` source-list style). ✅ (M2)
* Project lifecycle proto + daemon — `CreateProject`, `RenameProject`, `DeleteProject` + matching events. ✅
* Default shortcuts match the Go binary on macOS (`super+t`, `super+w`, `super+n`, `super+1..9`, `ctrl+1..9`, `super+shift+r`). ✅
* Keybind override config — Ghostty-style `keybind = trigger=action` in a config file, layered over defaults. ✅ (P1)
* WatchEvents subscription — sidebar/tab bar converge on daemon state without restart when other clients mutate. ✅ (M1)
* Secondary shortcuts: `font_increase`/`decrease`/`reset`. ✅ (P2). `cycle_tab_prev`/`cycle_tab_next`, `toggle_sidebar`, `rename_tab` deferred to `polish/secondary-shortcuts`.
* Visual polish — sidebar styling, tab bar styling, custom row backgrounds for the active item rather than the leading "● " marker. ✅ (M1 chrome + M2 sidebar + M3 tab pills)
* Selection + copy / paste in the terminal view. ✅ (M5)
* Window/terminal resize → cell-grid reflow + `PtyResize` over the existing `StreamPty` stream. ✅ (M3)
* Wide-char (CJK + emoji) cell width handling — the Mac renderer should consume libghostty-vt's per-cell width field. ⏳ deferred to `polish/wide-char-width`.

**Mergeability to main**: yes, throughout. Phase 6a is entirely additive — every commit leaves `cmd/` + `internal/` untouched.

## Goal

Bring the Mac UI to structural parity with the Go binary: a workable terminal multiplexer with projects, tabs, and the standard set of keyboard shortcuts users already have muscle memory for. After this phase the Mac UI is "almost done" minus the OSC routing differentiator (Phase 6b).

## Scope

In:
* Anything users routinely touch with the keyboard or mouse during normal multiplexer use.
* The keybind override config — users have config files today and want to keep them.
* WatchEvents subscription so a future cross-client scenario (CLI + UI both connected) stays consistent.

Out:
* OSC routing → Phase 6b.
* Linux UI → Phase 7.
* Bundling → Phase 8.

## Touches Go code?

No. Phase 6a is purely additive in `proto/`, `crates/roost-core/`, `crates/roost-cli-rs/`, and `mac/`.

## Step plan

### Done

* **Step 1 — Multi-tab** (`c161730`): tab bar + container with one `TerminalView` per tab. `⌘T`, `⌘W`, click-to-switch. NSMenu installed (App / File / Edit / Window). All tabs in this commit live under the daemon's auto-created default project.
* **Step 2a — Project lifecycle (proto + daemon)** (`0acc7b9`, fmt fixup `7a93125`): `CreateProject` / `RenameProject` / `DeleteProject` RPCs + `ProjectCreated` / `ProjectRenamed` / `ProjectDeleted` events. Daemon-side `Workspace::create_project`, `rename_project`, `delete_project` with broadcast emission. `roost-cli-rs project {list,create,rename,delete}` smoke-tested end-to-end. New Rust tests: `create_project_assigns_untitled_when_name_empty`, `rename_project_emits_event_and_persists`, `delete_project_cascades_tabs_and_emits_events`, `delete_project_promotes_fallback_active_selection`, `delete_project_unknown_returns_not_found`.
* **Step 2b — Project sidebar in Mac UI** (`57c20e1`): `NSSplitView` sidebar with project buttons; `+ New Project` button; right-click context menu (Rename… / Delete with confirmation alert). Tabs filtered into the sidebar-selected project. Active tab tracked by `TabSession` reference (not daemon tab id) so the marker is correct between OpenTab and the daemon's reply.
* **Step 2b polish — bootstrap + shortcut fixes** (`7a169ec`, `ee95ae0`):
  * `TabSession.init` now takes `projectID` — fixes the initial empty terminal + the "delayed ⌘T" bug where the filter ran before `start()` set the id.
  * Rebind `New Project` to `⌘N` (matches Go default).
  * `⌘1..⌘9` now switches **project** (was tab); `⌃1..⌃9` switches **tab** (matches Go).
  * Add `⌘⇧R` for `rename_project` against the active project.
  * Project switch auto-opens a tab in empty destinations so the terminal view never lingers on the previous project.
* **Misc polish**:
  * `d39e708` — Remove placeholder cell-grid overlay (was drawing on top of glyphs).

### Pending

* **Step 2c — WatchEvents subscription.** The Mac UI today fetches `listProjects` on launch and never resyncs. A second client (the CLI, or a future Linux UI) creating a project doesn't show up until the Mac UI restarts. Subscribe to `WatchEvents`, handle `ProjectCreated` / `ProjectRenamed` / `ProjectDeleted` / `TabOpened` / `TabDeleted` / `ActiveChanged` to keep the sidebar + tab bar in sync. Idle backpressure: gRPC's server-stream is unbounded from the daemon's `tokio::sync::broadcast` (capacity 256); a slow UI gets `Lagged` and should re-`listProjects` to resync. Specific work:
  * Add `watchEvents(socketPath:) async -> AsyncStream<Event>` to `RoostClient.swift`.
  * Spin one long-lived background `Task` in `RoostApp` that drains the stream and dispatches to `@MainActor` event handlers.
  * Handle each event variant by mutating the projects / tabs model + rebuilding sidebar / tab bar.
  * On `Lagged`, run `listProjects` again as a full resync.
* **Step 2d — Keybind override config.** Port the Ghostty-style `keybind = trigger=action` system from the Go side:
  * Read `~/.config/roost/config.conf` (or platform equivalent — match the Go binary's path; today both Mac and Linux use XDG even on macOS, see `spec.md`).
  * Reuse the Go binary's action-name namespace verbatim: `new_tab`, `close_tab`, `new_project`, `rename_project`, `rename_tab`, `cycle_tab_prev`, `cycle_tab_next`, `paste`, `copy`, `font_increase`, `font_decrease`, `font_reset`, `toggle_sidebar`, `switch_project_1..9`, `switch_tab_1..9`, `unbind`.
  * Port `cmd/roost/shortcuts.go::triggerToAccel` to Swift (the modifier alias rules are non-trivial — `super`/`cmd`/`command`, `ctrl`/`control`, `alt`/`option`/`opt`).
  * Port `cmd/roost/shortcuts.go::canonicalizeBindings` so a user's `keybind = cmd+t = unbind` correctly removes the `super+t` default.
  * Install the resolved bindings into the NSMenu at startup (each `NSMenuItem.keyEquivalent` + `keyEquivalentModifierMask` driven by the resolved action map).
  * Tests: take a copy of `cmd/roost/shortcuts_test.go`'s scenarios and port them.
* **Step 2e — Secondary shortcuts.** Each needs both the keybind config wiring (Step 2d) and the UI plumbing:
  * `cycle_tab_prev` / `cycle_tab_next` — easy, just selectTab(at: prev|next).
  * `font_increase` / `font_decrease` / `font_reset` — requires `TerminalView` to recompute cell metrics from a configurable font size, then re-layout. Medium.
  * `toggle_sidebar` — hide/show the left NSSplitView pane.
  * `rename_tab` — text-field dialog wired to `SetTabTitle` RPC with `user_titled=true`. Easy.
* **Step 2f — Selection + copy/paste.** TerminalView gains mouse tracking, cell-rectangle selection state, and a `copy` action that pulls plain text from libghostty-vt's render state for the selected region. Paste sends bytes (with bracketed-paste sequences when the shell asked for it via DECSET 2004). This is real renderer work.
* **Step 2g — Window resize.** `TerminalView` should reflow cols/rows on `viewDidEndLiveResize` (or `resizeSubviews(withOldSize:)`), call `ghostty_terminal_resize`, and send a `PtyResize` over the existing `StreamPty` stream so the PTY's winsize updates too.
* **Step 2h — Wide-char width.** Consume libghostty-vt's per-cell width field so CJK + emoji + ambiguous-width glyphs render correctly.
* **Step 2i — Visual polish.** A pass over the sidebar / tab bar to use NSBox / custom NSTableView appearance instead of `NSButton` + a `"● "` text marker. The Go GTK4 binary's polish bar is the rough target; "look at home on Mac" is the actual goal.

## Risks / known gaps

* WatchEvents subscription is the highest-priority remaining item — without it the sidebar diverges from the daemon's state silently. A user running `roost-cli-rs project create` from a terminal won't see it until they restart the Mac UI.
* Step 2d (keybind config) is well-defined because the Go binary already solves the problem — porting is mostly translation, but Swift's NSMenuItem accelerator surface is finicky (e.g. `keyEquivalent = "1"` + `keyEquivalentModifierMask: .control` is how you get `^1`, NOT `keyEquivalent = "\u{1}"`). Cover with tests.
* Step 2f (selection) is the biggest unknown — libghostty-vt's render state surface for "give me the text inside this cell rectangle" hasn't been exercised yet. May need a small helper on the Rust `roost-vt` crate that the Mac side can borrow patterns from.
* Step 2g (resize) is straightforward except the AutoLayout integration: today the `terminalContainer` has greaterThanOrEqualToConstant constraints; we need to switch to live-resize-driven recomputation.

## Order of operations (recommendation)

1. Step 2c (WatchEvents) — unblocks cross-client correctness, no UI ambiguity.
2. Step 2d (keybind config) — the user explicitly asked for it, well-scoped, port from Go.
3. Step 2e (secondary shortcuts) — small, lands incrementally as the override system makes each one easy.
4. Step 2g (window resize) — the biggest day-to-day quality-of-life gap.
5. Step 2h (wide-char width) — needed before non-ASCII users will be happy.
6. Step 2f (selection / copy) — biggest renderer work; saving for last lets us batch the necessary `TerminalView` plumbing in one pass.
7. Step 2i (visual polish) — natural last step, low risk, can happen in parallel with anything.

## Follow-ups

* The `~/.config/roost/config.conf` path on macOS is a deliberate XDG-style divergence from Apple HIG (see `spec.md`). The Swift config reader should respect the same path so users with existing configs don't have to move files at cutover.
