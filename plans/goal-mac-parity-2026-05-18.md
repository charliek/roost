# Goal — Mac UI parity (close gaps surfaced after Linux GTK parity push)

**Set**: 2026-05-18
**Closed**: 2026-05-22
**Owner**: Charlie Knudsen
**Co-author / executor**: Claude (Opus 4.7)
**Status**: ✅ closed
**Predecessors**: [`goal-linux-gtk-parity-2026-05-17.md`](goal-linux-gtk-parity-2026-05-17.md) (closed 2026-05-18 — Linux GTK is now the polished reference; Mac UI is the lagging surface).
**Successor**: [`phase-8-bundling.md`](phase-8-bundling.md) — the `feature/rust-port → main` merge gate.

## Closure (2026-05-22)

M1–M6 shipped; M7 (headerbar) dropped after user evaluation — Mac's existing inline `+ Tab` button, ⌘B sidebar toggle, and `File → Open Folder…` menu item are enough; an NSToolbar would have duplicated the affordances. Seven polish rounds (R1–R7) followed M1–M6 to settle drag/drop, inline rename, scroll-to-visible, pill width bounds, the resizable sidebar, and the `⌘B` collapse + drop-indicator UX.

| Milestone | Status | PR(s) |
|---|---|---|
| M1 — Live cwd subtitle | ✅ shipped | #67 |
| M2 — Tab drag-to-reorder | ✅ shipped | #68, polish in #71, #72, #74, #75, #76 |
| M3 — Sidebar drag-to-reorder | ✅ shipped | #68, polish in #71 |
| M4 — Tab pill right-click menu | ✅ shipped | #67, polish in #71 |
| M5 — Inline rename | ✅ shipped | #69, polish in #71, #72 |
| M6 — Sidebar rollup stripes | ✅ shipped | #67 |
| M7 — Headerbar | 🪦 dropped (user-eval) | — |
| R1–R7 polish rounds | ✅ shipped | #71, #72, #73, #74, #75, #76 |

**Next gate**: Phase 8 bundling. The Mac UI is feature-complete against the Go binary and the Linux gtk4-rs UI as of this closure.

## Context

The Linux GTK parity push (M1–M10 + M9.5) brought `crates/roost-linux/` up to visual + UX parity with the Go binary. A symmetric audit of `mac/Sources/Roost/` against the same Go binary surfaced a similar-sized gap list — the Mac UI predates the Linux push and several of the Linux milestones don't exist on Mac.

The user plans to use both Linux and Mac heavily; both need to feel finished before Phase 8 ships them.

**Cross-cutting invariant (carried from the Linux goal)**: UI state mutations always reconcile through `WatchEvents`; UIs never speculate locally. The Mac UI's existing `insertProjectLocallyIfMissing` dedupe at `mac/Sources/Roost/App.swift:591` is the reference pattern. Every milestone that mutates `projects` / `tabs` collections must follow this rule.

## Audit summary (2026-05-18)

| # | Item | Status | Where |
|---|---|---|---|
| 1 | Tab drag-to-reorder | ❌ Missing | no DnD on `TabPillView` |
| 2 | Sidebar drag-to-reorder | ❌ Missing | no DnD on NSOutlineView |
| 3 | Platform-detect keybinds | ✅ Present | `Keybind.swift:193`, full set including Mac-only ⌘⇧U |
| 4 | Inline rename (label↔entry) | ⚠️ Modal alert | uses `NSAlert` at App.swift:880, 1645 |
| 5 | Right-click context menu | ⚠️ Sidebar yes, tab pill no | `TabPillView` has no `menuForEvent` |
| 6 | Status icons + sidebar rollup stripes | ⚠️ Partial | tab pill dot ✅ (App.swift:1560), sidebar rollup ❌ (only binary "notifying" dot at App.swift:1996) |
| 7 | Sidebar "+ Project" footer button | ✅ Present | App.swift:331 |
| 8 | Headerbar buttons (folder, sidebar toggle, "+ Tab") | ❌ Missing | no `NSToolbar` installed |
| 9 | Window subtitle = live tab cwd | ⚠️ Bug | App.swift:1496 uses project's static cwd; `tabCwd` event never re-fires `updateWindowTitle()` |
| 10 | Visual polish (hover state, section header styling) | ⚠️ Partial | no `NSTrackingArea` on tab pills; section header is plain secondary-label text |
| 11 | Event handlers (server-driven convergence) | ⚠️ Partial — deliberate | `.tabOpened`/`.active` dropped (local-authoritative); `.tabsReordered`/`.projectsReordered`/`.hookActive` no-op `break` — blocks cross-client reorder convergence |
| 12 | Lifecycle hardening (exit→close, empty→window close) | ✅ Present | App.swift:641, 1124 |
| 12b | Spawn-failure UI teardown | ⚠️ Unverified | log-and-drop in `runShellSession`; orphan pill may stick around on `openTab` throw |
| 13 | ⌘N double-render (issue #57) | ✅ Fixed in code | App.swift:587–595. **GitHub issue still open — close it.** |
| 14a | File drag → new project | ❌ Missing | no `registerForDraggedTypes` |
| 14b | OSC 0/2/7 + notifications + theme config | ✅ Present | OscScanner, DesktopNotifications, Theme.loadBundled |
| 14c | User-supplied theme files | 🪦 dropped | same call as Linux M11 — bundled palettes are enough |

## Milestones (ordered by visible impact)

Per user direction (2026-05-18): headerbar icons are deferred to M7 so the user can evaluate whether they want a headerbar on Mac at all — Mac's existing inline `+ Tab` button + ⌘B sidebar toggle + folder-picker-via-`File → Open Folder` may already be enough. Live cwd subtitle stays at M1 because it's a strict bug fix (subtitle drifts from reality after every `cd`).

### M1 — Live cwd subtitle (OSC 7 → window subtitle)

**Branch**: `polish/mac-live-cwd-subtitle`
**Effort**: ~0.25 day

1. Fix `updateWindowTitle()` (App.swift:1496) to read the active tab's `session.liveCwd` instead of the project's static cwd.
2. Hook `tabCwd` event handler at App.swift:702 to call `updateWindowTitle()` after `rebuildTabBar()` so the subtitle follows `cd` via OSC 7.

**Exit criteria**:
* `cd /tmp` in a tab updates the window subtitle within ~1s.
* Switching between tabs in the same project updates the subtitle to that tab's cwd.

### M2 — Tab drag-to-reorder

**Branch**: `polish/mac-tab-drag-reorder`
**Effort**: ~0.75 day

`TabPillView` (App.swift:1832) becomes draggable via `mouseDragged` + `beginDraggingSession`. Implement `NSDraggingSource` (provide the tab id as `NSPasteboardItem` data) and `NSDraggingDestination` on the tab strip's scroll view (compute drop index from hit-testing, fire `reorderTabs` RPC).

Add `reorderTabs(projectID:, tabIDs:)` to `RoostClient.swift` (mirrors `reorder_tabs` already on the daemon).

Wire `.tabsReordered` event arm (App.swift:744) to actually reorder the pill views.

**Exit criteria**:
* Drag a tab pill, drop it elsewhere in the strip — order persists.
* `roost-cli-rs tab reorder --project-id $P --order ...` from a sibling terminal updates the Mac UI within ~1s.

### M3 — Sidebar drag-to-reorder

**Branch**: `polish/mac-sidebar-drag-reorder`
**Effort**: ~1 day

Wire `NSOutlineView` drag-and-drop:
* `outlineView(_:pasteboardWriterForItem:)` returns a `NSPasteboardItem` carrying the project id.
* `outlineView(_:validateDrop:proposedItem:proposedChildIndex:)` accepts moves within the projects section.
* `outlineView(_:acceptDrop:item:childIndex:)` reorders the data model + fires `reorderProjects` RPC.

Add `reorderProjects(projectIDs:)` to `RoostClient.swift`.

Wire `.projectsReordered` event arm to call `reloadData()` after applying the daemon's authoritative order.

**Exit criteria**:
* Drag a project row in the sidebar, drop it elsewhere — order persists across daemon restart.
* Cross-client convergence: `roost-cli-rs project reorder --order ...` updates the Mac UI within ~1s.

### M4 — Tab pill right-click context menu (Rename + Close)

**Branch**: `polish/mac-tab-pill-context-menu`
**Effort**: ~0.25 day

Override `menu(for:)` on `TabPillView`. Build an `NSMenu` with Rename + Close items, each carrying the tab id via `representedObject`. Rename triggers M5's inline rename; Close fires `closeTab` RPC.

**Exit criteria**:
* Right-click a tab pill, see Rename + Close.
* Close from the menu behaves identically to clicking the × on the pill.

### M5 — Inline rename (replace NSAlert with label↔entry stack)

**Branch**: `polish/mac-inline-rename`
**Effort**: ~1 day

Match the Go binary's UX:
* Sidebar row: `NSOutlineView` row view becomes a custom view with a `NSTextField` (label mode, default) + editable `NSTextField` (entry mode). Double-click flips to entry; Enter commits, Escape cancels. Replaces the modal alert at App.swift:880.
* Tab pill: hosts an inline editor over the pill on rename. Replaces the modal alert at App.swift:1645.

WatchEvents-only mutation: commit fires `renameProject` / `setTabTitle` RPC; label updates only after `ProjectRenamedEvent` / `TabTitleEvent` arrives. M9 Linux port is the reference.

**Exit criteria**:
* Double-click sidebar row → inline edit; Enter commits, Escape cancels.
* Double-click tab pill → inline edit; same.
* Concurrent CLI rename updates the label mid-edit without clobbering the user's in-progress text (matches Linux M9 invariant).

### M6 — Sidebar rollup stripes (priority-ranked indicator)

**Branch**: `polish/mac-sidebar-rollup`
**Effort**: ~0.5 day

`ProjectRowCellView` (App.swift:1979) gains a 3px left stripe colored by the per-project rollup state. Priority order matches Linux M6: `NEEDS_INPUT > ERROR > RUNNING > IDLE > NONE`. Hook the existing `.tabState` / `.hookActive` event arms to recompute the rollup. The binary "notifying" dot can stay (M4 Linux kept the per-tab dot too).

Colors: running blue (`#5fa3f0`), needs-input orange (`#f0a040`), idle gray (`#7a7a7a`), error red.

**Exit criteria**:
* Run `claude` in a tab; sidebar row stripe shifts blue → orange → gray as the agent cycles.
* Two tabs in one project: stripe color = higher-priority tab's state.

### M7 — Headerbar [user-evaluate] + cleanup + verify

**Branch**: `polish/mac-headerbar-and-cleanup`
**Effort**: ~1.25 day if user keeps the headerbar; ~0.5 day if dropped.

This milestone bundles a deferred-decision item with two small cleanup tasks. The user wants to evaluate whether a Mac headerbar is desirable at all before implementing — the Go binary's headerbar buttons are GTK-native UX; on macOS, the equivalent affordances may live more naturally in the menu bar or as system-wide gestures.

1. **Headerbar (defer decision until this milestone)**: install an `NSToolbar` with three items:
   * Folder picker → `NSOpenPanel.directoryURL` → `createProject(cwd:)`. Could also live as `File → Open Folder…` menu item (less screen real estate cost).
   * Sidebar toggle → fires existing `KeybindAction::toggleSidebar`. Could also stay keyboard-only (⌘B already exists).
   * `+ Tab` → fires `openTab` on the active project. The inline `+` in the tab strip (App.swift:381) already exists; headerbar version is redundant unless we move the inline one out.

   **Decision point**: drop the headerbar entirely, ship a partial subset (e.g., folder picker only via menu bar), or ship all three. Decide at the start of M7 based on look + feel during M1–M6 testing.

2. **Close GitHub issue #57** — the ⌘N double-render fix landed earlier at App.swift:587-595; only the issue state is stale.

3. **Verify spawn-failure UI teardown**: if `openTab` throws, the pending pill should tear down. If it doesn't today, add the equivalent of Linux's `fail_cleanup` (mark server-driven + remove pill).

4. **Optional**: file drag → new project (drop a folder on the sidebar to create a project with that cwd). Low priority — `File → Open Folder` covers the common path.

## Cross-cutting verification

**Two-axis verification**, same shape as the Linux goal:

1. **Visual inspection**: screenshot the Mac UI side-by-side with the Go binary on the same daemon, milestone by milestone.
2. **Automation (CLI)**: `roost-cli-rs project reorder` / `tab reorder` / `tab set-state` to exercise cross-client convergence — open the Mac UI + Linux UI simultaneously, drive state from CLI, watch both converge.

## Total horizon

| Milestone | Effort |
|---|---|
| M1 — Live cwd subtitle | 0.25 day |
| M2 — Tab drag-to-reorder | 0.75 day |
| M3 — Sidebar drag-to-reorder | 1 day |
| M4 — Tab pill context menu | 0.25 day |
| M5 — Inline rename | 1 day |
| M6 — Sidebar rollup stripes | 0.5 day |
| M7 — Headerbar [user-evaluate] + cleanup | 0.5–1.25 day |
| **Total serial** | **~4.25–5 days** |

## Branch / PR shape

Same per-milestone polish PR shape as the original goals (the Linux parity goal's "one bundled PR" was an explicit exception for chrome port cohesion). Each milestone gets its own `polish/mac-<name>` branch + PR; auto-merge into `feature/rust-port` gated on the 3 required macOS CI checks.

## Cross-cutting risks

* **NSToolbar item ordering with the macOS title bar accessory** — the existing `+ Tab` button lives inline in the strip; M7's headerbar (if shipped) would add a redundant header version. Decide at M7 whether to drop the headerbar entirely, keep both, or move the inline `+` out.
* **NSOutlineView reorder API is heavier than gtk4's** — the DnD round-trip through `NSPasteboardItem` is more ceremonial. Build a small Swift unit test against the data-model reorder math separate from the AppKit plumbing (mirrors Linux's `compute_insert_idx` test).
* **Mac UI's deliberate "local active state authoritative" stance** (App.swift:680) was a judgment call. M3 reverses that for `.projectsReordered` (we honor daemon-driven reorder events). Document the divergence in the milestone commit so the next reviewer doesn't trip on it.

## After this goal closes

* This goal doc gets `Status: ✅ closed` + a closure table summarising what landed.
* Phase 8 (bundling) becomes the next gate. The bundled DMG ships with the polished Mac UI.
