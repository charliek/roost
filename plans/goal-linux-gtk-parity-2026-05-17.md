# Goal — Linux GTK parity (port chrome + UX from Go GTK binary to gtk4-rs Rust UI)

**Set**: 2026-05-17
**Owner**: Charlie Knudsen
**Co-author / executor**: Claude (Opus 4.7)
**Status**: ✅ closed — M1–M10 + M9.5 lifecycle hardening all merged on `feature/rust-port` by 2026-05-18. M11 (user-supplied theme overrides) explicitly dropped after the user confirmed bundled theme parity is sufficient.
**Predecessors**: [`phase-7-linux-ui.md`](phase-7-linux-ui.md) (closed PR #50 squash `421b384`); [`phase-7-5-polish-and-gaps.md`](phase-7-5-polish-and-gaps.md) (this goal supersedes it — see "What's superseded vs carried forward" below).
**Successor**: [`goal-mac-parity-2026-05-18.md`](goal-mac-parity-2026-05-18.md) — the symmetric audit on the Mac Swift UI, then [`phase-8-bundling.md`](phase-8-bundling.md) for the `feature/rust-port → main` merge.

## Closure summary (2026-05-18)

All planned milestones either landed or were deliberately scoped out:

| Milestone | Status | PR | Notes |
|---|---|---|---|
| M1 — PTY spawn fix + multi-project bootstrap attach | ✅ landed | #51-#56 (per-milestone series, merged into `feature/rust-port`) | Diagnosed as Workspace-open orphan-purge interaction; fix preserves M5 cascade. |
| M2 — Window subtitle (live cwd via OSC 7) | ✅ landed | per-milestone PR | |
| M3 — CSS port + sidebar visual baseline | ✅ landed | per-milestone PR | |
| M4 — Sidebar footer `+ Project` button | ✅ landed | per-milestone PR | |
| M5 — Headerbar buttons (folder picker, sidebar toggle, `+ Tab`) | ✅ landed | per-milestone PR | Embedded SVGs (no GResource); GLib log filter for cosmetic warning. |
| M6 — Linux event handler completion | ✅ landed | per-milestone PR | Linux honors `ActiveChanged` / `HookActiveChanged` (Mac deliberately ignores these). |
| M7 — Status indicator icons + sidebar rollup stripes | ✅ landed | per-milestone PR | Rollup priority: `NEEDS_INPUT > ERROR > RUNNING > IDLE > NONE`. |
| M8 — Right-click context menus + new keybind actions | ✅ landed | #63 | Adds RenameProject/RenameTab/DeleteProject actions; `adw::AlertDialog` confirms. |
| M9 — Inline rename via `gtk::Stack` + `gtk::Popover` | ✅ landed | #63 | WatchEvents-only mutation, no optimistic local writes. |
| M9.5 — Lifecycle hardening (race fixes, focus polish, Go-parity chrome) | ✅ landed | #64 → squash `2029577` | Cmd+T double-spawn race, `exit`-doesn't-close, headerbar icons on dark theme, ForceDark scheme, focus return after rename. |
| M10 — Drag-to-reorder (sidebar projects + tabs) | ✅ landed | #66 → squash `5768943` | Live-shuffle sidebar via `gtk::DragSource`/`DropTarget`; `connect_page_reordered` for tabs; `ProjectsReordered` event wired. |
| M11 — User theme overrides | 🪦 dropped | — | All 7 bundled palettes already at parity in Linux UI; selection via `theme = <name>` in `config.conf` covers the actual use case. Mac and Go also lack user-supplied overrides. |

**What's next for the rust-port branch:**

1. **[Mac UI parity push](goal-mac-parity-2026-05-18.md)** — the Mac Swift UI predates the Linux parity work and lags on tab/sidebar drag-reorder, headerbar buttons, live cwd subtitle, inline rename, tab pill right-click, and sidebar rollup stripes. Audit findings + milestone plan live in that goal doc.
2. **[Phase 8 bundling](phase-8-bundling.md)** — the merge-to-`main` gate. Mac DMG + Linux AppImage. Does not block on Mac parity, but the user wants both UIs polished before the bundled artifacts go out.

---

## Original goal (preserved for context)

**Cross-cutting invariant (new, applies to every UI track)**: UI state mutations always reconcile through `WatchEvents`; UIs never speculate locally. The optimistic-append double-render that produced Mac issue [#57](https://github.com/charliek/roost/issues/57) is the canonical anti-pattern. Every milestone that mutates `projects` / `tabs` collections must follow this rule. The existing GTK attach-dedupe guard at `crates/roost-linux/src/app.rs::attach_existing_tab` (`if tabs.contains_key(&tab.id) { return; }`) is the reference pattern.

## Context

Phase 7 (PR #50, squash `421b384`) landed the Rust gtk4-rs Linux UI with all the structural pieces (cell renderer, key encoder, WatchEvents subscription, OSC scanner, themes, config). End-to-end testing during Phase 7 was happy-path: a single client against a fresh daemon, with the user confirming "yes, things work."

The user just did a fresh end-to-end test on a clean machine and the GTK app is visibly far from parity with the Go binary they're happy with:

* **Terminal area is blank** — the GTK app launches, shows the sidebar with the right projects, but no shell prompt renders. Log shows `StreamPty spawn failed: status: NotFound, message: "tab N not found"` followed by repeated `PTY exited status=0`. Without a working terminal every other polish item is unreviewable.
* **Sidebar chrome is bare** — no "Projects" section header, no `+ New Project` footer button, no status-indicator dots, no hover/selection styling, no inline rename, no right-click context menu.
* **No tab strip is visible** — AdwTabBar is wired in code (`crates/roost-linux/src/app.rs:313`) but renders empty because no tabs successfully attach (consequence of the PTY bug).
* **No headerbar buttons** — Go binary has folder picker + sidebar toggle + `+ Tab`; GTK port has none.
* **Window title** shows `Roost — <project>` only; Go binary + Mac UI both show a two-line title with the working directory as subtitle.

Phase 7.5 (`plans/phase-7-5-polish-and-gaps.md`) was scoped before this end-to-end test and covers most of the visual gaps (A1 CSS, A2 headerbar, A3 status indicators, A4 inline rename, B1 drag-reorder, D event handlers). What it **doesn't** cover is the PTY-spawn bug, window subtitle, right-click context menus, and the `+ New Project` button location mismatch (7.5 A2 puts `+` in headerbar; Go binary puts `+ New Project` at the sidebar footer).

The user wants the GTK app brought up to **visual + UX parity with the Go GTK binary**, using the Mac Swift UI as a secondary reference. Some Rust/gtk4-rs idiomatic divergence from Go is fine (e.g., AdwTabView's built-in reorder signals instead of Go's custom drag handlers).

The Mac UI also has a separate double-render bug on `⌘N` (issue [#57](https://github.com/charliek/roost/issues/57)), which the user will address in a separate PR. Out of scope here.

## Decision: new goal doc, supersedes Phase 7.5

User shaping (this conversation):

| Question | Choice |
|---|---|
| Plan shape | New `plans/goal-linux-gtk-parity-2026-05-17.md` that supersedes `plans/phase-7-5-polish-and-gaps.md`. |
| PTY-bug ordering | Diagnose + fix first; all other milestones depend on a working terminal for visual review. |
| `+ New Project` placement | Sidebar footer only (Go-parity). Drop the headerbar `+ Project` proposed in 7.5 A2. |
| PR shape | One PR with multiple commits (one per milestone), against `feature/rust-port`. Same shape as Phase 7's PR #50. |
| Verification | Two axes — visual inspection (screenshot + side-by-side with Go binary) AND automation interface (CLI `roost-cli-rs` + direct gRPC via `roost-smoke`). |

## Branch model + PR shape

Decision (recorded in plan, not derived from chat context): **one PR with multiple commits**, one commit per milestone, against `feature/rust-port`. Same shape as Phase 7's PR #50. The `CLAUDE.md` / `plans/README.md` default convention is per-milestone polish PRs (a la the M-/P-series predecessor goals); this goal **takes an explicit exception** for the bundled-review benefit on a tightly-coupled chrome port. Note the exception in `plans/README.md` when filing the repo-side goal doc.

* **Single topic branch**: `polish/gtk-parity` off `feature/rust-port`. All milestone commits land on it sequentially. Per-milestone branch names in the milestone sections below (e.g. `polish/gtk-pty-attach-fix`) are **commit-message prefixes**, not separate branches.
* **Commit cadence**: one commit per milestone (M1, M2, …). After each commit, push to `polish/gtk-parity`. The PR is opened in **draft** state after M1 lands so subsequent pushes accumulate without prematurely tripping auto-merge.
* **PR lifecycle**: draft → ready-for-review when user is happy with the M-set landed (typically after M7 checkpoint). Auto-merge is **disabled while draft** (GitHub disallows it) and enabled at the ready-for-review flip.
* **Merge target**: `feature/rust-port`. Squash-merge to a single commit; message body lists each milestone.
* **Auto-merge gates**: 3 required macOS checks per `feature/rust-port` branch protection — `test (macos-latest)`, `rust-build (macos-latest)`, `swift-mac` (verified via `gh api repos/charliek/roost/branches/feature/rust-port/protection`). The `gtk-build (macos-latest)` job is informational but **kept green as a plan invariant** since the entire goal is a GTK port — a non-required failure on the central thing must not silently rot.
* **CI cadence note**: push each milestone commit, **wait for CI on the head**, fix anything that breaks before stacking the next milestone. The single-PR shape means CI runs only against the head; a regression introduced in M4 that breaks `gtk-build` will only fail at M5+ push time if M5's diff is what trips the check. Cadence-discipline is how we catch silent rot.
* **Rebase cadence**: `git fetch origin && git rebase origin/feature/rust-port` at the start of each working session; force-push the topic branch.
* **Revert tradeoff**: a squashed single-PR commit cannot be partially reverted. If M9 or M10 turn out to be wrong post-merge, a follow-up PR is the remediation — not `git revert`. Accept this tradeoff explicitly.
* **CodeRabbit**: reviews the PR holistically at first ready-for-review flip; incremental on subsequent pushes. Address actionable items in fixup commits (squashed at merge); skip nits per CLAUDE.md.
* **Per-commit hygiene**: each milestone commit message starts with `M{N}: <subject>` (e.g. `M1: Diagnose + fix PTY spawn`) so the squash log reads as a phase narrative.
* **Bailout**: if M10's spike reveals the AdwTabView observing-side API doesn't deliver what's needed and a manual drag-handler proves > 2× the planned effort, ship M1–M9 + skip M10; file `polish/gtk-drag-reorder` as a follow-up branch.

## Reference repos

* `cmd/roost/` + `internal/` — Go GTK binary, the **primary visual + UX reference** (user is happy with it).
* `mac/Sources/Roost/` — Swift Mac UI, secondary reference for state machines + event handling.
* `../ghostty/` — libghostty-vt source, available for OSC + key-encoder reference if needed.
* `../cmux/` — only relevant for design ideas the user has cherry-picked; not a parity target.

## What's superseded vs carried forward from Phase 7.5

Carried into this goal:
* 7.5 A1 → **M3** (CSS port)
* 7.5 A2 → **M5** (headerbar buttons; trimmed — `+ Project` moved to sidebar footer in M4)
* 7.5 A3 → **M7** (status indicator icons + rollup stripes)
* 7.5 A4 → **M9** (inline rename)
* 7.5 A5 → **M11** (user theme overrides; optional, lowest priority)
* 7.5 B1 → **M10** (drag-to-reorder, Linux only)
* 7.5 D → **M6** (event handler completion)

Out of scope for this goal (split off as separate follow-up goals):
* 7.5 B2 (Mac drag-reorder) → `polish/mac-reorder-drag` follow-up; Mac is structurally complete, B2 is Mac polish not Linux parity.
* 7.5 C1–C4 (automation API gaps: tab snapshot, watch CLI, NotFound surfacing, fire_notification tracing) → file as `goal-automation-api-gaps-<date>.md` or fold into Phase 8.
* 7.5 E1–E4 (cross-platform polish: wide-char width, IME, mouse encoder, option-as-alt) → file as separate `polish/*` sibling goals; orthogonal to Linux GTK parity.

After this goal closes, `plans/phase-7-5-polish-and-gaps.md` gets marked `🪦 superseded — see goal-linux-gtk-parity-2026-05-17.md` with a pointer; uncarried tracks (B2/C/E) get explicit follow-up homes in `plans/README.md` open-questions section.

## Milestones

In a single-PR multi-commit shape, commits land sequentially regardless of logical independence. The dependency graph below is informational — for understanding what M7 needs from M6, etc. — not for parallelizing branches. **Corrections from Codex review**: M8 depends on M9's rename flow (its context-menu Rename triggers M9's flow); M2 / M11 are still independent.

### M1 — Diagnose + fix PTY spawn ("tab N not found") + Linux bootstrap multi-project attach [BLOCKER]

**Commit prefix**: `M1: ...`

**Problem A (PTY-spawn NotFound)**: The GTK app's `TabSession::spawn` (`crates/roost-linux/src/tab_session.rs:60`) sends a `PtyAttach { tab_id, cols, rows }` as the first message of the `StreamPty` bidi stream; the daemon's handler at `crates/roost-core/src/service.rs:325` (verified via Codex's read) resolves `workspace.tab(attach.tab_id)` against the SQLite store — **NOT** the in-memory PTY supervisor — and returns `Status::not_found("tab N not found")` when the **DB row** lookup misses. Log captured during the user's session:

```
01:16:31.609679Z INFO  roost_core::service: tab opened tab_id=1 project_id=2
01:16:31.760276Z INFO  roost_core::service: tab opened tab_id=1 project_id=3
01:16:31.730903Z WARN  roost_linux::app: StreamPty spawn failed tab 1 not found
(repeated ~5x)
01:16:32.051141Z INFO  roost_linux::app: PTY exited status=0
```

The **key clue** (CodeRabbit, Codex): two `tab opened tab_id=1` ~150ms apart across different projects is **only possible if the `tab` table was empty just before each insert**. With the schema using plain `id INTEGER PRIMARY KEY` (rowid alias, no `AUTOINCREMENT`; confirmed at `crates/roost-core/migrations/0001_init.sql`), rowids ARE reused after `DELETE FROM tab` when the table is empty. So either `Workspace::open` ran twice, two daemons are running, or something is wiping rows mid-bootstrap. WatchEvents is in-memory (recreated on daemon startup, `crates/roost-core/src/state.rs:115`; verified by Codex), so "buffered events from a prior session" is **not** a viable hypothesis.

**Problem B (multi-project attach — found by Codex panel review)**: independent of Problem A, `crates/roost-linux/src/app.rs:243` only attaches tabs for the **first** project on bootstrap. Persisted tabs in non-first projects won't render even with Problem A fixed. M1 fixes both.

**Important context — don't naively revert the orphan-purge**: commit `234378e` deliberately added `Workspace::open → Store::delete_all_tabs` to fix an M5 cascade misfire (stale tab rows blocked `delete_project` from firing when the user typed `exit` in the last visible tab). Removing it would regress that fix. Whatever M1 lands must preserve M5's cascade behavior.

**Investigation steps**:

0. **Step 0 — confirm singleton invariants**: `pgrep -fa roost-core` shows exactly one process; instrument `Workspace::open`, `Store::delete_all_tabs`, `Store::close_tab`, `Store::delete_project` with `tracing::info!` (id + caller location); confirm `Workspace::open` fires exactly once per daemon life. CodeRabbit specifically calls this out as the most likely shape of the bug.
1. **Repro recipe — DB delete BEFORE daemon start (Codex correction)**: kill all daemons → `rm -rf ~/Library/Application\ Support/roost/roost.db` → start daemon under `RUST_LOG=roost_core=trace,roost_linux=trace` → start GTK app alone (no Mac UI) → record the full log. The previous version of this plan listed DB deletion after daemon start, which is wrong — the daemon's live workspace would already hold the old state in memory.
2. Verify schema: `pragma table_info(tab)` shows `id` is plain `INTEGER PRIMARY KEY` (no `AUTOINCREMENT`); rowid reuse is therefore expected after empty-table state. (Confirmed in `crates/roost-core/migrations/0001_init.sql`.)
3. Trace what `tab_id` the GTK app is passing to `PtyAttach` — verify against the `TabOpenedEvent` it received via WatchEvents. Look at `crates/roost-linux/src/app.rs::attach_existing_tab` and the WatchEvents drain.
4. Read the daemon's `StreamPty` handler at `crates/roost-core/src/service.rs:300-380` end-to-end. Note: the `NotFound` is a SQLite miss at `service.rs:325`, **not** a runtime-supervisor miss. Any fix that targets the in-memory PTY map (e.g. the original plan's hypothesis (b)) cannot address this error — Codex proved this from the code.
5. Read `state.rs::open_tab` (around `:409` per Codex): the DB row is persisted and the runtime entry inserted **before** `TabOpenedEvent` is broadcast. So a PTY-runtime-vs-event race cannot produce a SQLite miss.

**Revised hypotheses**:

* **(a) Same-session row wipe**: something is calling `Store::delete_all_tabs` / `close_tab` / `delete_project` cascade in the bootstrap window, wiping rows the GTK app then tries to attach to. Combined with rowid reuse, the same `tab_id` can appear, get deleted, and be reused — the UI's previous attach attempt then finds the row "missing" because it was wiped, not because it never existed. Investigation step 0 catches this.
* **(b) GTK UI bootstrap ordering bug**: the GTK app subscribes to WatchEvents AND calls `ListTabs` on bootstrap; if it triggers attaches from both paths the second attach may target a row that was already torn down by a fast `close_tab`. Trace per step 3 confirms or rules out.
* **(c) Linux-bootstrap-only attaches first project** (Codex finding): even if (a) and (b) are fixed, tabs in projects 2..N never attach. **This is a bug regardless of A's root cause** and is part of M1's fix scope.

**Fix shape** — depends on root cause for Problem A; Problem B has a known fix:

* **For Problem B (always)**: iterate over **all** projects in `app.rs:243` bootstrap; attach each project's tabs. Add a unit/integration test covering "bootstrap with 2 projects × 2 tabs each renders all 4 terminals."
* **For Problem A**, depending on diagnosis:
  - If something is wiping the `tab` table mid-bootstrap: scope/defer the purge — option A1: gate `delete_all_tabs` behind a daemon-start-only watermark + skip after first client `Identify`; option A2: replace global wipe with "delete tabs whose `project_id` no longer exists in the project table" (preserves M5's cascade since live projects' tabs are kept; orphans get culled).
  - If the GTK bootstrap is firing duplicate attaches: dedupe at the UI side using the existing `attach_existing_tab` guard pattern (per the cross-cutting invariant in the header).
  - **Fallback workaround if root cause needs > 1 day**: have `TabSession::spawn` retry on `NotFound` with 100ms-200ms-400ms backoff (3x), then surface a UI-visible error. File the real fix as a follow-up issue.

**Test additions** (Kimi + Codex):
* `crates/roost-core/src/state_test.rs` (or wherever the existing `projects_survive_reopen_tabs_do_not` test lives, renamed if needed): add `bootstrap_attach_does_not_miss_after_orphan_purge` regression test that sequences `open`, fakes a UI's `OpenTab` + `StreamPty` attach, and asserts no `NotFound` even after a same-session `delete_all_tabs` or a `delete_project` cascade.
* `crates/roost-linux/src/app.rs`: add a small headless integration test for the multi-project bootstrap path (Problem B) — mock the client, list 2 projects with tabs in each, assert all tabs result in `attach_existing_tab` calls.

**Exit criteria**:
1. Fresh repro: kill all daemons → `rm ~/Library/Application\ Support/roost/roost.db` → start daemon → start GTK → default project shows a live shell prompt (cursor blinking, typing produces output) within ~2s.
2. **Multi-project**: create 2 projects with the CLI, each with a tab, restart GTK → both projects' tabs render.
3. **Daemon-restart regression**: stop and restart the daemon mid-session; both UIs reconnect within ~2s; **zero** `tab not found` lines in 60s of subsequent activity (CLI `tab send`, GTK keystrokes, sidebar drag — once M10 lands).
4. **M5 cascade still works**: type `exit` in the last visible tab → daemon-side `list_tabs(project_id)` is empty → `delete_project` fires → window closes.
5. Smoke run includes the regression test scripts from "Test additions" above; all green.

**Effort**: ~1–1.5 days (Codex's findings + the multi-project bug expand original scope from "diagnose A only").

### M2 — Window subtitle (cwd path)

**Commit prefix**: `M2: ...`

**Scope**:
1. Replace the current `gtk4::Label` headerbar title widget (set at `crates/roost-linux/src/app.rs:84-94`) with `adw::WindowTitle` so it supports `set_title` + `set_subtitle`.
2. `set_title` ← active project name. `set_subtitle` ← active tab's cwd (tilde-abbreviated; the OSC 7 handler at `app.rs:593-595` already tilde-abbreviates for the tab-pill label — extract that helper into a small `tilde_abbreviate(path) -> String` function).
3. Hook updates on: `set_active_project`, `set_active_tab`, `TabCwdChangedEvent`.
4. Format reference: verify `mac/Sources/Roost/App.swift::updateWindowTitle` exists with the format expected (Codex notes the Mac function should be confirmed before committing M2's scope to its format).
5. **Optional consolidation (CodeRabbit + Codex)**: the `tilde_abbreviate` helper exists separately in Linux and Mac code. Consider colocating in `crates/roost-common`; pure utility function with no FFI / GTK / AppKit dependency, doesn't violate decision-log DL-5 (the "no shared theme/keybind crate" rule). Land in M2's commit or as a separate `polish/roost-common-utilities` follow-up if rebase noise is a concern.

**Test additions**: `tilde_abbreviate` unit test (covers `/Users/charliek/foo` → `~/foo`, `/etc` → `/etc`, edge case of empty / root).

**Exit criteria**: header reads project name + tilde-abbreviated cwd; updates within ~1s of `cd` in the shell (OSC 7 path); unit test green.

**Effort**: ~0.5 day (0.75 day if `roost-common` consolidation is rolled into the same commit).

### M3 — CSS port + sidebar visual baseline

**Commit prefix**: `M3: ...`

**Scope** (matches Go binary verbatim per CodeRabbit + Codex cross-check against `cmd/roost/style.css`):
1. Port `cmd/roost/style.css` → `crates/roost-linux/src/resources/style.css`. **Use Go binary class names verbatim**, no invented names:
   * `.navigation-sidebar > row:selected` — accent_bg_color highlight.
   * `.navigation-sidebar > row:hover` — faint hover background.
   * `.sidebar-section-header` — small-caps, 0.78em, 55% opacity (consumed by M4's section-header widget). **Note**: the class name `.roost-projects-header` from the previous version of this plan was invented; the Go binary uses `.sidebar-section-header` (`cmd/roost/style.css:24`).
   * `tabbar > tabbox > tabboxchild:checked` — 18% accent-color tint.
   * `.roost-rollup-running` / `-needs-input` / `-idle` / `-error` — 3px `box-shadow: inset 3px 0 0 <color>`:
     - running → `#5fa3f0` (blue)
     - needs-input → `#f0a040` (orange)
     - idle → `#7a7a7a` (gray)
     - **error → red** (mirrors M6's `4/ERROR` state in the rollup priority — added per CodeRabbit + Kimi: previous version had M6 emit ERROR but M3 had no matching class).
2. Load via `gtk::CssProvider::load_from_string(include_str!("resources/style.css"))` in `App::new`; apply with `gtk::style_context_add_provider_for_display`.
3. **Graceful fallback**: if `CssProvider::load_from_string` returns an error (malformed CSS), log via `tracing::warn!` and continue with Adwaita defaults; don't panic. Verify this path by intentionally passing broken CSS in a test.

**Exit criteria**:
* Sidebar visual matches the Go binary screenshot (capture fresh: `./build/build.sh && ./roost` + AppleScript-frontmost screencap → diff against the gtk4-rs version side-by-side; no checked-in screenshot required since the Go binary is the live reference).
* All five rollup CSS classes reachable via `widget.add_css_class(...)` — M7 wires them to live state.
* App launches cleanly with intentional bad CSS injected and falls back to Adwaita defaults (test added in `crates/roost-linux/src/app.rs` or a new `style_test.rs`).

**Effort**: ~0.5 day.

### M4 — Sidebar footer "+ Project" button + Projects section label

**Commit prefix**: `M4: ...`

**Scope** (NEW vs Phase 7.5; Go-parity — labels and class names match Go verbatim per CodeRabbit):
1. Restructure the sidebar widget tree at `crates/roost-linux/src/app.rs:96-106` from a bare `gtk::ListBox` inside a `gtk::ScrolledWindow` to:
   ```
   gtk::Box(vertical)
     ├─ gtk::Label("Projects") with .sidebar-section-header CSS class
     ├─ gtk::ScrolledWindow → gtk::ListBox (current)
     └─ gtk::Button("+ Project") with .roost-add-project CSS class
   ```
   Button label is `+ Project` (matching `cmd/roost/app.go:270`), not `+ New Project`. Header CSS class is `.sidebar-section-header` (matching `cmd/roost/style.css:24`), not the invented `.roost-projects-header`.
2. Wire the button to `KeybindAction::NewProject` (already in `keybind.rs`).
3. Drop the `+ Project` headerbar button from M5's scope (it lived in 7.5 A2; this goal moves it).

**Exit criteria**: sidebar shows `Projects` header above the list, `+ Project` button anchored at the bottom; clicking opens a new project; visual matches Go binary screencap.

**Effort**: ~0.5 day.

### M5 — Headerbar buttons (folder picker, sidebar toggle, + Tab)

**Commit prefix**: `M5: ...`

**Scope** (Phase 7.5 A2, scope-trimmed; commits to vendored SVGs per Kimi feedback — drop the GResource-or-vendored ambiguity):
1. Vendor the SVGs from `cmd/roost/icons/` (`folder-symbolic.svg`, `sidebar-show-symbolic.svg`, `tab-new-symbolic.svg`) to `crates/roost-linux/src/resources/icons/`. Embed via `include_bytes!` + `gio::BytesIcon`, wrap in `gtk::Image::from_paintable`. Skip the GResource compilation step — vendoring SVGs as static bytes is simpler and removes one build-tool dependency.
2. Add headerbar buttons:
   * **Folder picker** (left of title) → opens `gtk::FileChooserDialog`; chosen path passed to `NewProject` RPC as cwd.
   * **Sidebar-toggle button** (left) → fires existing `KeybindAction::ToggleSidebar`.
   * **+ Tab button** (right of title) → fires `NewTab` on the active project.
3. Filter the cosmetic `g_settings_schema_source_lookup: assertion 'source != NULL' failed` GLib warning via `glib::log_set_writer_func` (Go binary already does this in `cmd/roost/loghandler.go`; port the filter).

**Exit criteria**:
* Buttons render with icons on Mac Homebrew GTK4 + on a real Linux GNOME install without `adwaita-icon-theme` in `XDG_DATA_DIRS` (one of the M5 acceptance steps is to spot-check on a Linux host — VM or container is fine).
* Startup log no longer carries the cosmetic GLib warning. Verify via: `./target/debug/roost-linux 2>&1 | grep -i 'g_settings_schema_source_lookup'` returns empty.

**Effort**: ~0.5 day.

### M6 — Linux event handler completion [blocks M7]

**Commit prefix**: `M6: ...`

**Scope** (Phase 7.5 D). **Important framing correction (Codex)**: the Mac UI currently **ignores** `.hookActive`, `.tabsReordered`, `.projectsReordered`, and treats local active state as authoritative (`mac/Sources/Roost/App.swift:680`). So describing M6 as "mirror the Mac UI" is wrong — Linux honoring `ActiveChanged`, `HookActiveChanged` etc. is a **new UX choice**, not parity. The choice is justified separately: cross-client convergence requires the Linux UI to honor daemon-driven state changes when a CLI mutation arrives. Document the divergence in the milestone commit message.

1. Handle `TabStateChangedEvent` in `crates/roost-linux/src/app.rs::handle_event` — currently falls through the `_ =>` arm. State machine:
   * `0/NONE` → no indicator
   * `1/RUNNING` → blue
   * `2/NEEDS_INPUT` → orange
   * `3/IDLE` → gray
   * `4/ERROR` → red
2. Per-project rollup aggregation: **priority order corrected** (Kimi): `NEEDS_INPUT > RUNNING > ERROR > IDLE > NONE` was inconsistent with the per-tab state palette (where ERROR sits between RUNNING and IDLE on urgency). Settled order: `NEEDS_INPUT > ERROR > RUNNING > IDLE > NONE`. Expose as `project_rollup(project_id) -> RollupState` so M7 can consume.
3. Handle `HookActiveChangedEvent` — when a tab's hook is active, suppress visual urgency (don't promote RUNNING/NEEDS_INPUT colors to the indicator icon picker). Per-tab `hook_active: bool` field consulted by M7.
4. Handle `ActiveChangedEvent` — sync the GTK UI's `active_project_id` / `active_tab_id` with the daemon's notion when it changes via a CLI mutation (so `roost-cli-rs tab focus --tab N` from a sibling terminal also focuses on the GTK UI).

**Test additions** (Kimi + Codex):
* New `crates/roost-linux/src/rollup.rs` (or inlined as a module in `app.rs`) housing the rollup state machine as a pure function: `pub fn project_rollup(tab_states: &[(TabState, bool /* hook_active */)]) -> RollupState`. Unit-test priority ordering for all 10 combinations explicitly: empty list, all-NONE, single-RUNNING, single-ERROR, NEEDS_INPUT-wins-over-RUNNING, NEEDS_INPUT-wins-over-ERROR, ERROR-wins-over-RUNNING, hook-active-suppresses-RUNNING, hook-active-suppresses-NEEDS_INPUT, mixed.

**Exit criteria**:
* `roost-cli-rs tab set-state --tab N --state running` → handler fires; `tracing::info!` log line on receive; M7's indicator picker (once landed) updates.
* Unit tests for rollup priority all green.
* `roost-cli-rs tab focus --tab N` from a terminal sibling shifts the GTK UI's active tab visibly.

**Effort**: ~1 day.

### M7 — Status indicator icons + rollup stripes [depends on M3 + M6]

**Commit prefix**: `M7: ...`

**Scope** (Phase 7.5 A3):
1. Embed 4 status SVGs from `cmd/roost/icon_running.svg` / `icon_needs_input.svg` / `icon_idle.svg` / `icon_error.svg` via `include_bytes!`; wrap each in `gio::BytesIcon` and cache (mirror Go binary's `cmd/roost/indicator.go`). (Previous version of plan listed only 3 icons; ERROR was missed.)
2. On `TabStateChangedEvent` (M6's handler routes it): `tab_ui.page.set_indicator_icon(Some(&icon))` with matching SVG. AdwTabPage's built-in `set_indicator_icon` is verified in libadwaita 0.8.1 (Codex confirmed via cargo registry read).
3. On rollup change (M6's aggregation API): apply CSS class to the sidebar row, removing all rollup classes first:
   ```rust
   for cls in ["roost-rollup-running", "roost-rollup-needs-input", "roost-rollup-idle", "roost-rollup-error"] {
       row.remove_css_class(cls);
   }
   if let Some(cls) = rollup.css_class() {
       row.add_css_class(cls);
   }
   ```

**Test additions**: rollup priority test already in M6; M7 just consumes the API.

**Exit criteria** (Codex correction — replace weak "run `claude`" criterion with deterministic CLI-driven transitions):
* `roost-cli-rs tab set-state --tab $T --state running` → AdwTabPage shows the blue running icon within ~500ms.
* Repeat for `needs_input` → orange, `idle` → gray, then a manually-injected `error` state → red. Each transition observable on the GTK UI without restarting.
* **Sidebar rollup**: open two tabs in one project, set tab A to `running` + tab B to `needs_input`; sidebar row shows the `needs-input` (orange) stripe because it outranks running per M6's priority. Set both to `idle` → stripe goes gray.
* **Cross-client**: same RPC sequence with the Mac UI as a second client; cross-client convergence (Mac shows its own indicator after the same RPCs).
* Dogfood backup: run `claude` in a tab and visually confirm the live cycle, but this is not the primary acceptance.

**Effort**: ~1 day.

### M8 — Right-click context menus + new keybind actions

**Commit prefix**: `M8: ...`

**Scope** (NEW vs Phase 7.5; Go-parity):
1. **Keybind dispatch — these actions do NOT exist today** (Codex + CodeRabbit verified; previous plan version's "if missing" hedge was wrong). Add to `crates/roost-linux/src/keybind.rs`:
   * `KeybindAction::RenameProject` (default `Ctrl+Shift+R`)
   * `KeybindAction::RenameTab` (default `Ctrl+R`)
   * `KeybindAction::DeleteProject` (no default — requires explicit confirmation, so a stray keypress can't dataloss; advanced users bind via config)
   These are owned by M8; M9 just dispatches into the rename flow.
2. Sidebar rows: attach `gtk::GestureClick` (button 3) at construction (`app.rs::add_project_ui`). On secondary-click, show `gtk::PopoverMenu` with:
   * **Rename** → triggers M9's rename flow on this row.
   * **Delete** → confirmation via `adw::AlertDialog` (Codex: not `gtk::MessageDialog`; `adw::AlertDialog` is the libadwaita-current pattern matching Go binary's `cmd/roost/app.go:2035`). On confirm, fire `DeleteProject` RPC.
3. Tab pills: AdwTabView has `connect_setup_menu` for per-page context menus. Hook with:
   * **Rename** → triggers M9's rename flow on this tab.
   * **Close** → fires `CloseTab` RPC (existing behavior, just exposed via menu).
4. Right-clicks on the "Projects" section header (M4) and the `+ Project` footer button are no-ops (Kimi).

**Exit criteria**:
* `Ctrl+Shift+R` opens rename on the active project's row (after M9 lands; M8's exit can land before M9 if rename target is stubbed to a `tracing::info!`).
* Right-click on a sidebar row pops a `Rename` / `Delete` menu; `Delete` triggers the `adw::AlertDialog` confirmation; confirm fires `DeleteProject` RPC.
* Right-click on a tab pill pops `Rename` / `Close`; `Close` fires `CloseTab` RPC.
* Right-click on the section header / footer button is a no-op.

**Effort**: ~0.75 day.

### M9 — Inline rename via gtk::Stack + gtk::Popover

**Commit prefix**: `M9: ...`

**Scope** (Phase 7.5 A4; reflects the cross-cutting "WatchEvents-only mutation" invariant in this plan's header):
1. **Sidebar rows**: port `cmd/roost/project_row.go`'s inline-rename pattern. Each row's label is wrapped in a `gtk::Stack` (`label` ↔ `entry` children). Double-click the label, or trigger `KeybindAction::RenameProject` (added in M8), or trigger M8's context-menu Rename → flip the Stack to entry. Enter commits via `RenameProject` RPC; Escape cancels.
2. **Tab pills**: Use a `gtk::Popover` over the pill (Codex correction: `connect_setup_menu` is the **context-menu** API on AdwTabView, not an inline-rename hook; AdwTabPage has no built-in rename). The Go binary's tab-rename uses popover-over-pill at `cmd/roost/app.go` — port that pattern. Trigger: double-click the pill, M8's context-menu Rename, or `KeybindAction::RenameTab`. Enter commits via `SetTabTitle` RPC; Escape closes the popover.
3. **No optimistic local mutation** (cross-cutting invariant): when the user commits, fire the RPC and **wait for `ProjectRenamedEvent` / `TabTitleEvent`** from WatchEvents before updating the label. The existing `attach_existing_tab` guard pattern (`if tabs.contains_key { return; }`) is the reference. **Exit-criterion test**: if a CLI rename arrives via WatchEvents while the user is mid-edit in the inline entry, the label updates **but the Stack stays on Entry**; user's in-progress text is not clobbered.
4. **Race-guard test**: M9 commits **a deduplicate-on-event regression test** (in `crates/roost-linux/src/app.rs` or a new test module) that simulates a rapid double-commit (RPC + same-id event) and asserts only one render-state change. The Mac issue [#57](https://github.com/charliek/roost/issues/57) is the canonical anti-pattern this test guards against.

**Test additions**:
* Rapid-rename race regression (above).
* Stack state preservation: ensure WatchEvents `ProjectRenamedEvent` arriving while Stack is on `entry` updates the underlying label data but leaves the visible Stack child as `entry` (test via mock event injection).

**Exit criteria**:
* Double-click a sidebar row → inline entry appears; Enter commits, Escape cancels.
* Double-click a tab pill → popover with rename entry; Enter commits via `SetTabTitle` RPC, Escape closes.
* `KeybindAction::RenameProject` / `RenameTab` shortcuts work as alternative entry points.
* Race-guard test green.
* No duplicate sidebar rows / tab pills after rapid commits (issue #57 anti-pattern check).

**Effort**: ~1 day.

### M10 — Drag-to-reorder UI

**Commit prefix**: `M10: ...`

**Scope** (Phase 7.5 B1). **API correction (Codex)**: `connect_page_reordered` and `connect_setup_menu` **do exist** in libadwaita 0.8.1 (verified via cargo registry read at `~/.cargo/registry/src/.../libadwaita-0.8.1/src/auto/tab_view.rs:527`); the Phase-7.5 doc's "API uncertainty" framing was stale. No spike required — wire `connect_page_reordered` directly. AdwTabBar handles the drag UX itself (built-in); the **observing-side** is what M10 wires.

The **applying-side** API (`reorder_page(&page, target_index)`) is already used at `crates/roost-linux/src/app.rs:559` to handle inbound `TabsReorderedEvent` — keep that path.

1. **Sidebar reorder (the bigger lift)**: attach `gtk::DragSource` to each `gtk::ListBoxRow` (project) + `gtk::DropTarget` to the parent `gtk::ListBox`. On drop, compute the target order, fire `ReorderProjects` RPC. **Wire the currently-stubbed `EventKind::ProjectsReordered(_)` arm at `app.rs:564`** — currently a no-op; M10 makes it call `sidebar.reorder` or equivalent to apply the daemon's authoritative order. The Go binary at `cmd/roost/app.go:854` does live row shuffling that preserves active selection while moving; mirror that — drop-then-snap-back is jarring.
2. **Tab reorder**: hook `tab_view.connect_page_reordered`; on the signal, compute the new tab-id sequence via `tab_view.pages()` iteration, fire `ReorderTabs` RPC. AdwTabBar's built-in drag handles the visual reorder; the signal fires after the user-driven move completes.
3. **Cross-project tab drag**: **out of scope** (needs a new daemon RPC + cross-project move semantics). File `polish/gtk-cross-project-drag` follow-up.
4. **Multi-client race**: if user A reorders while user B also reorders, `ReorderProjects` is last-write-wins; document this as acceptable.

**Test additions**:
* Reorder-index math unit test in `crates/roost-linux/src/app.rs` (or a new `reorder.rs` module): given a starting `Vec<i64>` and a `(source_index, target_index)` pair, the new order vector matches expected (test edge cases: drop-on-self, drop-at-end, drop-at-start, drop-between-adjacent).

**Exit criteria**:
* Drag a sidebar project row → drop in new position → live shuffle preserves active-row selection during the drag, persists on drop, daemon log line includes `projects_reordered` with the correct id sequence; Mac UI (as second client) reflects the new order via WatchEvents within ~1s.
* Drag a tab pill → reorder; daemon log `tabs_reordered`; same convergence.
* **Persistence**: `roost-cli-rs project list` returns ids in the new order; restart `roost-linux` → sidebar shows the new order on bootstrap (proves the daemon persisted, not just emitted-then-forgotten).
* Reorder-math unit test green.

**Effort**: ~1.5 days (live-shuffle UX is the bulk of the work; sidebar drag handlers are mechanical once index math is settled).

### M11 — User theme overrides [optional]

**Commit prefix**: `M11: ...`

**Scope** (Phase 7.5 A5):
1. Scan `~/.config/roost/themes/<name>` at startup; any file there overrides the bundled theme of the same name. Use existing `parse_theme` at `crates/roost-linux/src/theme.rs:112`.
2. Hot reload: SKIP for this milestone — restart-only is fine (matches Mac UI).
3. Mac parity: same scan in `mac/Sources/Roost/Theme.swift::loadBundled` fallback chain.

**Exit criteria**: drop `~/.config/roost/themes/my-theme` with ghostty-format content, set `theme = my-theme` in `config.conf`, restart UI → palette applies. Bundled themes still load when no user override exists.

**Effort**: ~0.75 day. **Optional** — defer if time-boxed. **If deferred, file as `plans/polish/gtk-user-themes.md` follow-up before closing this goal** (Kimi: matches the CLAUDE.md "no TODOs left in committed code" rule, extended to plan-level scope).

## Total horizon

Estimates **revised after panel review** — M1 expanded to include the multi-project bootstrap bug Codex found; M2 bumped slightly for the optional `roost-common` consolidation; M10 stays since spike is no longer needed (signals exist):

* M1: 1–1.5 days (includes Problem A diagnosis + Problem B fix + regression tests)
* M2: 0.5 day (0.75 if `roost-common` extraction rolled in)
* M3: 0.5 day
* M4: 0.5 day
* M5: 0.5 day
* M6: 1 day
* M7: 1 day
* M8: 0.75 day
* M9: 1 day
* M10: 1.5 days (no spike needed; live-shuffle UX is the work)
* M11: 0.75 day (optional)

Serial total: **~10 days** with M11; ~9 without. **M1 + M2 + M3 + M4 + M5 + M6 + M7 = ~5.5 days** delivers a recognizably Go-parity GTK app on the chrome + status axis; M8 + M9 + M10 add the discoverability + reorder UX (~3.25 more days); M11 is a sometimes-nice-to-have.

## Dependency graph

```
M1 (PTY fix + multi-project attach) ── blocks every visual review ─────────────┐
M2 (subtitle)       ─── independent                                            │
M3 (CSS)            ─── independent ─── consumed by M4 (header class), M7 ────┐│
M4 (sidebar footer) ─── consumes M3's .sidebar-section-header class           ││
M5 (headerbar)      ─── independent                                          │││
M6 (events)         ─── consumed by M7 ──────────────┐                       │││
M7 (indicators)     ←────────────────────────────────┴──←─────────────────  ┘││
M8 (context menu)   ─── depends on M9's rename flow ─────────┐                ││
M9 (inline rename)  ─── consumed by M8 ──←────────────────── ┘                ││
M10 (drag-reorder)  ─── independent                                            ││
M11 (user themes)   ─── independent                                            ││
```

* **M1** is the hard prereq for visual review of every other milestone. Land it first.
* **M3** supplies CSS classes M4 (sidebar header) and M7 (rollup) consume.
* **M6** supplies the event-handler hooks M7 needs.
* **M9** owns the rename flow; **M8** depends on M9 (its menu Rename → M9 flow). M8 can land with a stubbed rename-trigger and M9 fills it in, but the user-visible Rename menu doesn't work until M9 lands.

## Critical files to be modified

* `crates/roost-linux/src/app.rs` — every milestone except M11 touches this; main UI orchestrator.
* `crates/roost-linux/src/tab_session.rs` — M1 (PTY attach diagnosis; possibly fix).
* `crates/roost-linux/src/terminal_view.rs` — only if M1's fix lands here (unlikely; renderer itself is sound).
* `crates/roost-linux/src/keybind.rs` — M8 (new action names), M9 (already has RenameTab/RenameProject).
* `crates/roost-linux/src/resources/style.css` — **new file** (M3).
* `crates/roost-linux/src/resources/icons/*.svg` — **new files** (M5, M7).
* `crates/roost-linux/src/theme.rs` — M11.
* `crates/roost-core/src/state.rs` and/or `crates/roost-core/src/service.rs` — M1 if root cause is daemon-side.
* `mac/Sources/Roost/Theme.swift` — M11 (Mac parity scan).
* `plans/goal-linux-gtk-parity-2026-05-17.md` — **new file** (this plan in repo form).
* `plans/phase-7-5-polish-and-gaps.md` — mark superseded with pointer.
* `plans/README.md` — status snapshot + open-questions update.

## Functions / patterns to reuse

* `crates/roost-linux/src/app.rs::handle_event` — central WatchEvents dispatcher; M6 fills in `TabStateChanged`/`HookActiveChanged`/`ActiveChanged` arms here, doesn't introduce a parallel dispatcher.
* `crates/roost-linux/src/theme.rs::parse_theme` — M11 reuses; same ghostty format the bundled themes use.
* `crates/roost-linux/src/keybind.rs::canonicalize_bindings` — M8 piggybacks on this for new actions, doesn't introduce a parallel parser.
* `mac/Sources/Roost/App.swift::updateWindowTitle` — M2's format reference.
* `cmd/roost/style.css` — M3's source CSS (paths/colors directly portable; rule selectors may need adapter for libadwaita widget names).
* `cmd/roost/project_row.go` — M9's inline-rename pattern.
* `cmd/roost/indicator.go` — M7's icon cache pattern.
* `cmd/roost/loghandler.go` — M5's GLib log-filter pattern.

## Verification

Two verification axes — **visual inspection** + **automation interface (CLI + gRPC)** — applied after each milestone commit lands, with reinforced end-to-end smokes at the M1, M5, M7, M10 checkpoints. Both axes are required: visual catches "does it look right," automation catches "does the wire contract still hold."

### Setup (each verification session)

**Order matters** (Codex correction): delete the DB **before** starting the daemon. The daemon caches workspace state in memory at `Workspace::open` time; deleting `roost.db` while the daemon is already running will not reset its in-memory state.

```bash
# Step 1 — kill any prior daemon
pkill -f 'target/.*/roost-core' || true

# Step 2 — clean slate state (skip if testing a specific persisted scenario)
rm -rf ~/Library/Application\ Support/roost/roost.db

# Step 3 — start daemon (Terminal 1)
cargo run -p roost-core

# Step 4 — start GTK UI (Terminal 2 — this is the thing under test)
./target/debug/roost-linux

# Step 5 — start Mac UI for cross-client convergence (Terminal 3, optional)
./mac/scripts/bundle.sh debug   # if Mac side changed
open mac/build/Roost.app

# Step 6 — start Go binary for direct visual A/B comparison (Terminal 4)
./build/build.sh && ./roost
```

Build `roost-cli-rs` once at the start of a session: `cargo build -p roost-cli-rs --release && export ROOST=./target/release/roost-cli-rs`.

**Pairing tip** (CodeRabbit): three UIs simultaneously means three subscribers all receiving `broadcast_structural_resync` on attach; cross-drags can thrash. Prefer pairing comparison one at a time: **GTK vs Go for visual diff** OR **GTK + Mac for convergence**, not all three at once.

### Axis 1 — Visual inspection (per milestone)

For each milestone, screenshot the GTK UI side-by-side with the Go binary, deterministically frontmost.

**macOS host** (the user's primary dev env):
```bash
# Bring GTK app frontmost by pid, then capture.
GTK_PID=$(pgrep -f 'target/release/roost-linux')
osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $GTK_PID) to true"
sleep 0.5
screencapture -o -x /tmp/roost-debug/gtk-mN.png

# Bring Go binary frontmost (process name "roost").
osascript -e 'tell application "System Events" to set frontmost of process "roost" to true'
sleep 0.5
screencapture -o -x /tmp/roost-debug/go-mN.png
```

**Linux host** (CodeRabbit: the AppleScript path is Mac-only; mirror equivalent for real-Linux validation, especially M5's icon-theme check):
```bash
# X11/GNOME: wmctrl + gnome-screenshot; Wayland/Sway: grim + swaymsg.
GTK_PID=$(pgrep -f 'target/release/roost-linux')
wmctrl -i -a "$(wmctrl -lp | awk -v pid="$GTK_PID" '$3==pid {print $1; exit}')"
sleep 0.5
gnome-screenshot -w -f /tmp/roost-debug/gtk-mN.png

# Same pattern for the Go binary process name "roost".
```

Diff against the Go binary screenshot side-by-side.

* **M1**: shell prompt visible in the GTK terminal area; cursor blinking; typing produces output.
* **M2**: header reads two-line `<project> / <cwd>`; updates within ~1s of `cd` in the shell (OSC 7 path).
* **M3**: sidebar selection accent, hover, "Projects" header styling matches `cmd/roost`'s Go GTK chrome.
* **M4**: `+ New Project` button anchored at sidebar bottom; "Projects" label above the list.
* **M5**: headerbar shows folder picker + sidebar toggle + `+ Tab` icons; no `g_settings_schema_source_lookup` warning on startup.
* **M7**: open a tab, run `claude` in it → indicator icon cycles blue → orange → gray; sidebar rollup stripe color tracks the per-project highest-priority tab.
* **M8**: right-click a sidebar row → popover with Rename + Delete; right-click a tab pill → Rename + Close.
* **M9**: double-click a sidebar row → inline `gtk::Entry`; Enter commits, Escape cancels.
* **M10**: drag a sidebar project row → drops in a new position; drag a tab pill → reorders.
* **M11**: drop a `~/.config/roost/themes/my-theme` → restart UI → palette applies.

### Axis 2 — Automation (CLI)

The `roost-cli-rs` binary (`crates/roost-cli-rs/src/main.rs`) exposes the daemon's full RPC surface: `identify`, `project {list, create, rename, delete, reorder}`, `tab {open, close, list, send, resize, focus, set-state, clear-notification, reorder}`, `notify`, `set-title`. Use it to drive each milestone's exit criteria headlessly (no UI required) — this catches wire-contract regressions a visual sweep would miss.

**Important** (Codex correction): the CLI prints **plain text**, not JSON, for `project create` / `tab open` etc. Don't pipe to `jq` — parse with `awk` / `grep` / `sed` instead. Also, `--bytes` arguments need real escape decoding: use `$'...'` bash quoting (or `printf` + pipe) so `\n` is interpreted as a newline.

Reusable smoke script (extend per milestone):

```bash
# M1 — PTY attach (the keystone). CLI returns plain text; extract id with awk.
PROJECT_ID=$($ROOST project create --name "Smoke" --cwd "$PWD" \
  | awk '/^id:/ {print $2; exit}')
TAB_ID=$($ROOST tab open --project-id "$PROJECT_ID" --title smoke \
  | awk '/^id:/ {print $2; exit}')

# Verify the tab is registered:
$ROOST tab list

# Write to the PTY — needs $'...' or printf to convert \n to a real newline.
# 'echo hello\n' literally sends backslash-n; $'echo hello\n' sends a newline.
$ROOST tab send --tab "$TAB_ID" --bytes $'echo hello\n'

# Visual: GTK terminal area shows "hello" within ~200ms.
# Daemon log assertion: should NOT print "tab N not found" anywhere.

# M1 daemon-restart regression — kill and restart while GTK is attached:
pkill -f 'target/.*/roost-core'
cargo run -p roost-core &
sleep 1
# GTK UI re-attaches via WatchEvents reconnect within ~2s; verify same TAB_ID still
# accepts $ROOST tab send and renders output.

# M6/M7 — state machine (no UI required to drive the daemon)
$ROOST tab set-state --tab "$TAB_ID" --state running
$ROOST tab set-state --tab "$TAB_ID" --state needs_input
$ROOST tab set-state --tab "$TAB_ID" --state idle
# Visual: indicator icon cycles blue → orange → gray; rollup stripe on the
# project row changes color in sync per M6's priority table.

# M10 — reorder convergence
$ROOST tab reorder --project-id "$PROJECT_ID" --order "$TAB_ID_B,$TAB_ID_A"
# Visual: GTK UI reflects new order via WatchEvents; if Mac UI is running,
# it converges too.
$ROOST project reorder --order "$P_B,$P_A"

# Notification path (M7 indirectly tests this)
$ROOST notify --tab "$TAB_ID" --title "Test" --body "Body"
# Visual: desktop notification fires; tab pill shows "needs attention" badge.

# Identify + cleanup
$ROOST identify
$ROOST project delete --id "$PROJECT_ID"
```

For convergence checks specifically, pair **GTK + Mac** against the same daemon, issue an RPC from the CLI, watch both converge. This mirrors the Phase 7 closure-verification pattern.

**Note about `roost-smoke`** (Codex correction): `crates/roost-smoke` is **not** a general low-level gRPC harness today — it's an interactive `OpenTab + StreamPty` smoke client (`crates/roost-smoke/src/main.rs`). This goal does NOT require new RPCs, so all automation goes through `roost-cli-rs`. If a future milestone needs richer gRPC drive (e.g. WatchEvents subscription verification headlessly), plan a `roost-smoke watch` subcommand at that time; out of scope here.

**Visual-only milestones** (M3, M4, M5, M9, M10 inline rename / drag UX): the automation axis cannot fully exercise these — they're pure UI chrome / drag UX / inline edit flow. The plan's "two-axis required" stance still applies (CLI drives state changes that the UI displays), but visual inspection is the dominant signal for these. Acceptance for these milestones documents that.

### Acceptance checklist per milestone commit

Before pushing a milestone commit:
1. Build green: `cargo build -p roost-core -p roost-linux -p roost-cli-rs` + `cd mac && PROTOC_PATH=$(which protoc) swift build`.
2. Tests green: `cargo test -p roost-linux -p roost-core -p roost-cli-rs` (roost-linux test surface is currently thin per Phase 7 inventory; we don't grow it gratuitously, but if a milestone adds a state machine — e.g., M6's rollup aggregation — add a unit test for it).
3. Visual smoke (axis 1) passes for that milestone.
4. Automation smoke (axis 2) passes for that milestone.
5. CodeRabbit's pre-existing comments (if any) addressed or explicitly tagged as out of scope.

### CI

3 required checks per branch protection: `test (macos-latest)`, `rust-build (macos-latest)`, `swift-mac`. The `gtk-build (macos-latest)` job is informational but **kept green as a plan invariant** — see Branch model section.

## Open questions / risks

* **M1 root cause is uncertain until traced.** Time-box the diagnosis at 1 day; if it stretches longer, ship the retry-backoff workaround (`TabSession::spawn` retries on `NotFound` with 100/200/400ms delays, 3x) + file a follow-up issue with the trace data so it doesn't block downstream milestones.
* **Cross-cutting "WatchEvents-only mutation" invariant** (header). The Mac double-render bug ([#57](https://github.com/charliek/roost/issues/57)) is the canonical anti-pattern. The same race risk exists in M9 (rename) AND in M1's `open_new_tab_in()` path per Codex (`crates/roost-linux/src/app.rs:365`). Both M1 and M9 explicitly guard. Consider promoting this invariant to `CLAUDE.md` or `vision.md` if it re-emerges in another track.
* **Hot-reload theme watching (M11)** — out of scope by design; revisit only if the user asks for it.
* **Cross-project tab drag** (M10 out of scope) — needs a new daemon RPC. File as `polish/gtk-cross-project-drag` follow-up if the user wants it.
* **`adw::WindowTitle` format mismatch on Mac vs Linux** (M2): the Mac UI uses AppKit `window.title` / `window.subtitle`; verifying `mac/Sources/Roost/App.swift::updateWindowTitle` exists with the format we'll mirror is a M2 pre-flight.
* **GTK CI is informational, not required.** The plan invariant "keep `gtk-build` green" hinges on cadence discipline (push commit → wait for CI → fix → next commit). Consider promoting `gtk-build (macos-latest)` to required-green at the end of the goal, before merge, if Phase 8 will rely on it.

## After this goal closes

* `plans/phase-7-5-polish-and-gaps.md` → marked superseded; the uncarried tracks (B2 Mac drag-reorder, C automation API, E cross-platform polish) get explicit homes in `plans/README.md` Open-Questions section.
* Phase 8 (bundling) is the next gate; this goal does not block it but should land before it so the bundled Mac DMG + Linux AppImage ship with the parity-feeling Linux UI rather than the current bare version.
* Mac issue #57 (double-render on ⌘N) tracked separately by the user.
