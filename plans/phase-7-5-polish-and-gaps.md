# Phase 7.5 — Polish + Automation Gaps

**Status**: ⏳ scoped, not started
**Set**: 2026-05-17
**Owner**: Charlie Knudsen
**Co-author / executor**: Claude (Opus 4.7)
**Predecessor**: [`phase-7-linux-ui.md`](phase-7-linux-ui.md) — closed 2026-05-17 via PR #50 squash `421b384`
**Successor**: [`phase-8-bundling.md`](phase-8-bundling.md) — the gate for `feature/rust-port → main`

## Problem

Phase 7 landed the full gtk4-rs Linux UI with multi-project sidebar + AdwTabView tab bar, cell renderer, full key encoder, scrollback + selection + clipboard, keybind config, OSC + notifications, themes + config. End-to-end tested side-by-side with the Go GTK4 binary + Swift Mac UI on Mac Homebrew GTK4; cross-client convergence verified.

But several follow-up items were deliberately deferred from Phase 7's commit budget so the ~3500-LOC PR didn't balloon. They split into five tracks:

* **Track A — Linux visual polish.** The gtk4-rs UI functions but doesn't yet match the Go binary's visual chrome (CSS rollup stripes, headerbar icons, AdwTabPage indicator icons, inline rename UX). User decision in the planning round was "use the Go GTK app as the primary visual reference" — Track A delivers on that.
* **Track B — Drag-to-reorder UI.** Phase 7 commit 3 added the daemon `ReorderTabs` + `ReorderProjects` RPCs and CLI surfaces. Neither the Linux UI nor the Mac UI has the drag handlers wired yet; both consume `TabsReorderedEvent` / `ProjectsReorderedEvent` from `WatchEvents` (Phase 7 commit 8 wires the Linux side) but the user can only reorder via the CLI today.
* **Track C — Automation API gaps.** Surfaced during the end-to-end test session on 2026-05-17:
  * `tab send` silently returns exit 0 on a tab with no live PTY (daemon emits `NotFound` but CLI swallows it).
  * `tab snapshot` RPC was deferred in Phase 6 M4 — would let CI verify renderer state headlessly. Architectural decision pending (daemon-side libghostty parse vs UI-side snapshot via StreamPty extension).
  * No `roost-cli-rs watch` subcommand to dump the `WatchEvents` stream — useful for debugging cross-client convergence.
  * Daemon-side `fire_notification` emits no log line — every other RPC handler logs at INFO so test verification has a paper trail.
* **Track D — Linux event handler completion.** The daemon emits `TabStateChangedEvent`, `HookActiveChangedEvent`, and `ActiveChangedEvent`; the Phase 7 Linux UI's `handle_event` falls through on all three. The Mac UI handles them (status indicator color + hook-active suppression). Without Track D the Linux sidebar / tab strip can't show a status-indicator dot.
* **Track E — Deferred cross-platform items.** Carried forward from earlier goal docs:
  * `polish/wide-char-width` (Phase 6a step 2h) — CJK + emoji per-cell width via libghostty's `wide_tail` field. Both UIs respect `wide_tail` for cursor skipping but don't yet allocate two cells of horizontal space for wide chars.
  * `polish/ime-composition` — Korean / Japanese / Chinese IME composition needs Ghostty's `markedText` pattern. Affects both UIs.
  * Mouse-encoder bindings in `roost-vt` for xterm mouse-tracking apps (`htop -mouse`, `vim` with `set mouse=a`). Currently the wheel handler defers in mouse-tracking mode; no clicks/drags propagate.
  * Option-as-Alt config setting on Mac — `GHOSTTY_KEY_ENCODER_OPT_MACOS_OPTION_AS_ALT` is hard-coded to "alt" today; some users want "esc" or "none".

## Decision (proposed)

Same process as the predecessor polish goals: long-lived `feature/rust-port`, short-lived `polish/*` topic branches, auto-merge gated on the 4 macOS CI checks (`test (macos-latest)`, `rust-build (macos-latest)`, `swift-mac`, `gtk-build (macos-latest)`). CodeRabbit reviews each PR.

| Question | Choice |
|---|---|
| Branch model | `polish/*` topic branches → `feature/rust-port` (same as M-/P-series predecessors). |
| CI gates | macOS-only required (the 4 above). Linux jobs informational until commit 8-equivalent surface mass. |
| PR shape | One topic branch per milestone, **not** one mega-PR. Phase 7's "one PR with multiple commits" approach worked but ran ~3500 LOC + multiple bug fixes mid-stream; the smaller-PR cadence keeps CodeRabbit's review surface tighter and lets the user merge per-track. |
| Ordering | Tracks A, B, C, E are independent of each other. **A3 depends on D** (A3's rollup CSS class + AdwTabPage indicator icon consume `TabStateChangedEvent`, which Track D wires up); land D before or alongside A3. A and B are the user-visible wins; C improves CI's test reach; D is polish completeness + a prereq for A3; E is cross-platform parity. |
| Stop condition | Once Tracks A + B + (optionally D) close, the Linux UI looks + behaves like the Go binary at the parity level the user signed off on. C + E can stretch into Phase 8 or beyond without blocking. |

## Branch shape

```
feature/rust-port  (current HEAD: 421b384, Phase 7 closed)
  ├── polish/linux-css                   (Track A — A1 CSS + sidebar styling)
  ├── polish/linux-headerbar-icons       (Track A — A2 GResource icons + headerbar buttons)
  ├── polish/linux-status-indicators     (Track A — A3 AdwTabPage status icons + rollup stripes)
  ├── polish/linux-inline-rename         (Track A — A4 Stack(label↔entry) for tabs + projects)
  ├── polish/linux-user-themes           (Track A — A5 ~/.config/roost/themes/<name>)
  ├── polish/linux-reorder-drag          (Track B — B1 Linux gtk drag source/target)
  ├── polish/mac-reorder-drag            (Track B — B2 Mac AppKit drag source/target)
  ├── polish/tab-snapshot-rpc            (Track C — C1)
  ├── polish/cli-watch-events            (Track C — C2)
  ├── polish/cli-tab-send-error          (Track C — C3 surface daemon NotFound)
  ├── polish/notify-tracing              (Track C — C4 fire_notification info log)
  ├── polish/linux-event-completion      (Track D — TabState/HookActive/ActiveChanged)
  ├── polish/wide-char-width             (Track E — E1)
  ├── polish/ime-composition             (Track E — E2)
  ├── polish/mouse-encoder               (Track E — E3)
  └── polish/option-as-alt               (Track E — E4)
```

When the tracks the user wants are closed, Phase 7.5 lifts to ✅. Skipping tracks is fine — they stay tracked in this doc as named follow-up branches.

## Out of scope (this goal)

* Phase 8 — bundling (notarytool + DMG + AppImage). Separate phase doc.
* Phase 9 — cutover (delete `cmd/` + `internal/`). Separate phase doc.
* Shared-code consolidation (`roost-theme` / `roost-config` / `roost-keybind` shared crates). DL-5 still says no; revisit only if drift accumulates. Quick alternative: cross-language Rust → Swift FFI via cbindgen. Tracked as a separate exploration if it comes up.
* cmux-tier features: notification rings, multiline sidebar metadata, in-app browser, splits, SSH workspaces, command palette.
* CodeRabbit nits intentionally not addressed in Phase 7 (magic numbers in `Error::from_result`, redundant `if` after `match` arm in OSC scanner). Low-value, skip unless the surface becomes load-bearing.

## Milestones

### Track A — Linux visual polish

#### A1 — CSS port from `cmd/roost/style.css`

PR target: `feature/rust-port`. Branch: `polish/linux-css`.

**Scope:**
1. Port `cmd/roost/style.css` → `crates/roost-linux/src/resources/style.css`. Rules to port:
   * Sidebar selection (accent_bg_color on selected row).
   * Sidebar hover (faint background on unselected rows).
   * Sidebar section header ("Projects" small-caps, 0.78em, 55% opacity).
   * Tab bar checked-tab accent tint (18% opacity).
   * Project rollup stripes (3px `box-shadow: inset 3px 0 0 <color>`):
     - `roost-rollup-running` → #5fa3f0 (blue)
     - `roost-rollup-needs-input` → #f0a040 (orange)
     - `roost-rollup-idle` → #7a7a7a (gray)
2. Load via `gtk::CssProvider::load_from_resource` or `gtk::CssProvider::load_from_string(include_str!(...))` in `App::new`.
3. Apply with `gtk::style_context_add_provider_for_display`.

**Exit criteria:**
* Sidebar selection visually matches the Go binary's screenshot at `plans/screenshots-2026-05-16/go-gtk-ui.png` (re-screenshot if needed).
* `roost-rollup-*` CSS classes can be added/removed via `widget.add_css_class(...)` — Track D wires that to live `TabState` changes.

**Effort**: ~0.5 day.

#### A2 — GResource headerbar icons + headerbar buttons

PR target: `feature/rust-port`. Branch: `polish/linux-headerbar-icons`.

**Scope:**
1. Register the GResource bundle from Phase 7 commit 0 (`cmd/roost/icons/icons.gresource`) in `App::new` via `gtk::IconTheme::for_display(...).add_resource_path("/dev/charliek/roost/icons")`. The four Adwaita SVGs (`folder-symbolic`, `sidebar-show-symbolic`, `sidebar-show-right-symbolic`, `tab-new-symbolic`) become reachable to `gtk::Image::from_icon_name`.
2. Add headerbar buttons mirroring the Go binary (`cmd/roost/app.go`):
   * Folder picker (left of title) → opens a `gtk::FileChooserDialog` for new-project cwd selection.
   * Sidebar-toggle button → toggles sidebar visibility (already in `ToggleSidebar` action; needs a button-trigger).
   * `+ Tab` button (right of title) → fires `NewTab` action on the active project.
3. Filter the cosmetic `g_settings_schema_source_lookup: assertion 'source != NULL' failed` GLib message — same approach the Go binary used in `cmd/roost/loghandler.go` (intercept `g_log_writer_default` via `glib::log_set_writer_func` and drop the matching line).

**Exit criteria:**
* Buttons render with their icons on Mac Homebrew GTK4 + on a Linux GNOME installation that doesn't have `adwaita-icon-theme` in `XDG_DATA_DIRS` — the GResource bundle handles both.
* Startup log no longer carries the cosmetic GLib assertion.

**Effort**: ~0.5 day.

#### A3 — AdwTabPage status indicator icons + rollup stripes

PR target: `feature/rust-port`. Branch: `polish/linux-status-indicators`.

**Depends on Track D** (the `TabStateChangedEvent` handler that A3 consumes lives there). Land D first, or stack A3 on top of D's branch. Without D, the events arrive at `handle_event` but fall through the catch-all `_ =>` arm.

**Scope:**
1. Embed the 3 status SVGs from `cmd/roost/icon_*.svg` via `include_bytes!` and wrap each in a `gio::BytesIcon`. Mirror the Go binary's `cmd/roost/indicator.go` cache.
2. On every `TabStateChangedEvent` (routed by Track D's `polish/linux-event-completion`), call `tab_ui.page.set_indicator_icon(Some(&icon))` with the matching SVG.
3. On every `TabState` change, update the **project's** rollup CSS class on the sidebar row:
   * Any tab with `RUNNING` → `roost-rollup-running` on the row.
   * Any tab with `NEEDS_INPUT` → `roost-rollup-needs-input` (precedence over running).
   * All tabs `NONE`/`IDLE` → `roost-rollup-idle`.
   * Aggregation happens in Track D's state machine.

**Exit criteria:**
* Visual: open a tab + run `claude` in it. Status indicator on the AdwTabPage cycles through running → needs-input → idle as the agent works.
* Sidebar rollup stripe color tracks the project's most-urgent tab state.

**Effort**: ~1 day. Most of the work is in the state aggregation; the icon wiring is mechanical once Track D's events are routed.

#### A4 — Inline rename via `gtk::Stack(label↔entry)`

PR target: `feature/rust-port`. Branch: `polish/linux-inline-rename`.

**Scope:**
1. Port `cmd/roost/project_row.go`'s inline-rename pattern to the Linux UI: a `gtk::Stack` per row toggles between a `gtk::Label` and a `gtk::Entry`. Double-click (or `KeybindAction::RenameProject` / `RenameTab`) flips the Stack to entry; Enter commits + fires `RenameProject` RPC; Escape cancels.
2. Tabs: same pattern on AdwTabPage's title via `tab_view.connect_setup_menu` or a custom popover. The Go binary doesn't inline-rename tabs (it uses a popover); pick whichever fits the gtk4-rs surface better.
3. Add `KeybindAction::RenameTab` + `RenameProject` to the action dispatch table.

**Exit criteria:**
* Double-clicking a sidebar row enters rename mode; the entry commits via Enter, cancels via Escape.
* `KeybindAction::RenameProject` (default Ctrl+Shift+R) opens rename on the active project's row.
* `KeybindAction::RenameTab` (default Ctrl+R) opens rename on the active tab.

**Effort**: ~1 day.

#### A5 — User theme overrides + hot reload

PR target: `feature/rust-port`. Branch: `polish/linux-user-themes`.

**Scope:**
1. Scan `~/.config/roost/themes/<name>` at startup; any file there overrides the bundled theme of the same name. Use the existing `parse_theme` from `crates/roost-linux/src/theme.rs`.
2. (Optional) Hot reload: `inotify` (Linux) / `FSEventStream` (Mac) on the themes dir → reload + re-apply via `Terminal::set_color_*`. The Mac UI ships restart-only theme load; matching that is fine for a first cut.
3. (Mac parity) Same scan on `~/.config/roost/themes/` in the Swift UI — drop into a `mac/Sources/Roost/Theme.swift::loadBundled` fallback chain.

**Exit criteria:**
* Drop `~/.config/roost/themes/my-theme` with ghostty `key = value` content, set `theme = my-theme` in `config.conf`, restart UI → palette applies.
* Bundled themes still load when no user override exists.

**Effort**: ~0.75 day (no hot reload) or ~1.5 days (with hot reload).

### Track B — Drag-to-reorder UI

#### B1 — Linux drag handlers

PR target: `feature/rust-port`. Branch: `polish/linux-reorder-drag`.

**Scope:**
1. **Sidebar drag-source + drop-target**: attach `gtk::DragSource` to each `gtk::ListBoxRow` (project) and `gtk::DropTarget` to the parent `gtk::ListBox`. On drop, compute the new order from the source + target row indices and fire `RoostClient::reorder_projects(...)`. WatchEvents-driven `ProjectsReorderedEvent` is the convergence path.
2. **AdwTabView reorder**: adw::TabView already has built-in drag-to-reorder via `tab_view.set_default_icon` + the framework's `connect_reorder_page_finish` (need to verify API name) signal. Hook the signal: when the user drops a tab, compute the new tab-id order and fire `RoostClient::reorder_tabs(project_id, &[...])`.
3. **Cross-project tab drag**: out of scope for this milestone — flagged as a separate `polish/cross-project-drag` follow-up. Requires a new daemon RPC (move tab between projects) which doesn't exist yet.

**Exit criteria:**
* Drag a sidebar project row → drop above another → daemon log shows `projects reordered`; both Linux + Mac (once Track B B2 lands) UIs reflect the new order via WatchEvents.
* Drag an AdwTabPage → drop in a new position → daemon log shows `tabs reordered`; same convergence.

**Effort**: ~1.5 days.

#### B2 — Mac drag handlers

PR target: `feature/rust-port`. Branch: `polish/mac-reorder-drag`.

**Scope:**
1. NSOutlineView drag-source + drop-target on the sidebar (`NSOutlineViewDataSource` `outlineView(_:writeItems:to:)` + `outlineView(_:validateDrop:proposedItem:proposedChildIndex:)`).
2. Tab pill drag-source: each TabPillView becomes draggable; on drop reorder via `RoostClient.reorderTabs(...)`.
3. Cross-project tab drag: same out-of-scope flag as B1.

**Exit criteria:**
* Visual parity with the Go binary's reorder UX.
* Cross-client convergence: drag-reorder on Mac → Linux UI updates within ~1s.

**Effort**: ~1.5 days. (AppKit drag APIs are well-trodden but verbose.)

### Track C — Automation API gaps

#### C1 — `tab snapshot` RPC

PR target: `feature/rust-port`. Branch: `polish/tab-snapshot-rpc`.

**Scope:**
1. New `TabSnapshot { tab_id, format }` RPC in `proto/roost.proto`. `format` enum: `TEXT_PLAIN`, `JSON`.
2. **Implementation choice**:
   * **Option A** — link libghostty-vt in the daemon, walk the render state daemon-side. Architecturally heavier; breaks the "daemon is libghostty-free" decision (DL-equivalent). ~2 days.
   * **Option B** — UI-side snapshot via the existing `StreamPty` bidi: client sends `PtySnapshotRequest`, UI responds with `PtySnapshotResponse` carrying the walked render state. Daemon brokers. ~1.5 days, lighter daemon, but couples snapshot availability to UI being attached.
   * **Option C** — daemon maintains an opt-in ring buffer of PTY output bytes (no parse). Snapshot returns the raw bytes; the caller parses or just greps. ~0.5 day; useful for "did the shell emit X?" tests, not for cell-level verification. Cheapest path.
   * Recommend **Option C** as the first cut + flag Option B as a follow-up if cell-level verification needs surface.
3. CLI surface: `roost-cli-rs tab snapshot --tab N --format plain|json` prints to stdout.

**Exit criteria:**
* `roost-cli-rs tab send --tab N --bytes 'echo hello\n' && sleep 0.5 && roost-cli-rs tab snapshot --tab N` → output contains `hello`.
* CI `crates/roost-smoke/` test uses snapshot to assert renderer correctness without a UI.

**Effort**: ~0.5–2 days depending on option.

#### C2 — `roost-cli-rs watch` for WatchEvents

PR target: `feature/rust-port`. Branch: `polish/cli-watch-events`.

**Scope:**
1. New subcommand: `roost-cli-rs watch [--tab N]`. Subscribes to the daemon's `WatchEvents` stream and pretty-prints each event to stdout. Pipes nicely into `jq` if the user uses `--format json`.
2. Useful for debugging cross-client convergence + for the test harness used during Phase 7 end-to-end validation.

**Exit criteria:**
* Run `roost-cli-rs watch` in one terminal; run `roost-cli-rs project create --name foo` in another → first terminal prints `ProjectCreated` event.

**Effort**: ~0.5 day.

#### C3 — Surface daemon `NotFound` from `tab send`

PR target: `feature/rust-port`. Branch: `polish/cli-tab-send-error`.

**Scope:**
1. Investigate: in `crates/roost-cli-rs/src/main.rs`'s `Cmd::Tab(TabCmd::Send { .. })` handler, `client.tab_write(...).await?` should propagate `tonic::Status` — but during the 2026-05-17 test session, `tab send --tab <dead>` exited 0 with no message. Either:
   * The daemon is returning `Ok` even when no PTY exists (most likely — verify in `crates/roost-core/src/service.rs::tab_write`).
   * OR the CLI is swallowing the error somewhere upstream of the `?`.
2. Fix: surface the daemon's `NotFound` to the user with a one-line error message + non-zero exit code. Pattern: same as the existing `project delete` error path.

**Exit criteria:**
* `roost-cli-rs tab send --tab 99999 --bytes 'x'` → stderr message "tab 99999 not found (no live PTY)", exit code != 0.
* `roost-cli-rs tab send --tab <live> --bytes 'echo x\n'` keeps working.

**Effort**: ~0.5 day, mostly debugging.

#### C4 — Daemon-side `tracing::info!` for `fire_notification`

PR target: `feature/rust-port`. Branch: `polish/notify-tracing`.

**Scope:**
1. One-liner in `crates/roost-core/src/state.rs::fire_notification`: log `tab_id` + `title` (truncated to 80 chars) at INFO.
2. Mirrors the pattern already used by `project created`, `tab opened`, `tabs reordered`, etc.

**Exit criteria:**
* `roost-cli-rs notify --tab N --title T --body B` → daemon log contains an INFO line.

**Effort**: ~5 minutes. Could be folded into C2's branch.

### Track D — Linux event handler completion

PR target: `feature/rust-port`. Branch: `polish/linux-event-completion`.

**Scope:**
1. **`TabStateChangedEvent`** → wires into Track A3's AdwTabPage indicator icon + sidebar rollup CSS class. Also surface via tab-strip pill color if the AdwTabPage's built-in indicator doesn't cover everything.
2. **`HookActiveChangedEvent`** → mirror the Mac UI's behavior of suppressing the tab's notification surface while a Claude hook owns it. Today the daemon already suppresses OSC 9/777-driven notifications during hook-active windows (Phase 6b P5); Track D adds the UI-side reflection (visual hint, e.g. dimmed indicator icon).
3. **`ActiveChangedEvent`** → reflect the daemon's notion of "active project/tab" if it changes via a CLI mutation. Today the Linux UI maintains its own `active_project_id` independently; this milestone optionally syncs them.

**Exit criteria:**
* `roost-cli-rs tab set-state --tab N --state running` → AdwTabPage indicator icon shows the blue running circle within ~1s.
* Claude-hook session start → Linux UI suppresses OSC notifications during the hook window (matches Mac UI).

**Effort**: ~1 day. Bulk of the work is the state aggregation for the sidebar rollup.

### Track E — Deferred cross-platform items

Lightweight tracking — each is its own follow-up branch. Effort estimates rough.

#### E1 — `polish/wide-char-width`

CJK + emoji per-cell width. Phase 6a step 2h. Both UIs already respect `wide_tail` in cursor positioning; this milestone allocates two cells of horizontal space for wide chars in the renderer (cell-skip logic + bg/glyph painting). ~1 day total across Mac + Linux.

#### E2 — `polish/ime-composition`

Korean / Japanese / Chinese IME composition via Ghostty's `markedText` pattern (cf. Ghostty's macOS Sources). Both UIs need the marked-text overlay + a commit-on-IME-confirm flow. ~2-3 days; the Mac side is heavier (NSTextInputClient protocol).

#### E3 — Mouse encoder

`roost-vt` mouse-encoder bindings (`ghostty_mouse_encoder_*`). UI-side: forward `GestureClick` / `EventControllerMotion` through the encoder to the PTY. Enables `htop -mouse`, `vim` with `set mouse=a`, `tig`'s mouse navigation. ~1.5 days.

#### E4 — Option-as-Alt config

Mac-only. Expose `GHOSTTY_KEY_ENCODER_OPT_MACOS_OPTION_AS_ALT` as a config-file key (`macos-option-as-alt = alt | esc | none`). One-line plumbing through `mac/Sources/Roost/KeyEncoder.swift`. ~30 minutes.

## Total horizon

* Track A: ~3.75 days (A1 0.5 + A2 0.5 + A3 1 + A4 1 + A5 0.75).
* Track B: ~3 days.
* Track C: ~1.5–3.5 days depending on `tab snapshot` option chosen.
* Track D: ~1 day.
* Track E: ~5.5 days if everything lands; mostly E1 + E2 + E3 + E4.

End-to-end if all tracks land serially: ~15 days. Realistically the user picks a subset — Tracks A + B + D are the visible-impact set; ~8 days. C and E can stretch beyond Phase 8 without blocking.

## Process

* Same `polish/*` topic branch model as the predecessor goals. `gh pr create --base feature/rust-port`; `gh pr merge --auto --squash`. Auto-merge gates on the 4 macOS required checks.
* CodeRabbit auto-reviews each PR via `.coderabbit.yaml`.
* Each milestone log entry below gets a one-paragraph summary on merge.

## Open questions / risks

* **`tab snapshot` architecture (C1)**. Option A (libghostty in daemon) vs Option B (UI broker) vs Option C (ring buffer). Recommend Option C as the first cut; user calls the shot at PR-open time.
* **AdwTabView native reorder API**. gtk4-rs 0.10 exposes `tab_view.connect_setup_menu` and reorder-related signals; the exact API for "I want to know the user reordered a page" needs a quick spike during B1.
* **Hot-reload file watching (A5)**. Adding `notify` crate as a dependency for the optional hot-reload path is fine on Linux (inotify) but cross-platform on Mac means rebuilding the watcher logic per-OS. Start with restart-only theme load; hot reload can be a sub-commit if there's appetite.
* **Cross-project tab drag** (B1/B2 deferred): the daemon needs a new RPC to move a tab between projects. Track as a separate `polish/cross-project-drag` if user wants it; not in 7.5 scope.
* **Wide-char widths (E1)**: affects both UIs but the implementations diverge (Pango layout cell-skip on Linux vs Core Text on Mac). Track per-UI in the PR description.

## Milestone log

(Empty — fill in as PRs merge.)

* A1 — pending.
* A2 — pending.
* A3 — pending.
* A4 — pending.
* A5 — pending.
* B1 — pending.
* B2 — pending.
* C1 — pending.
* C2 — pending.
* C3 — pending.
* C4 — pending.
* D  — pending.
* E1 — pending.
* E2 — pending.
* E3 — pending.
* E4 — pending.
