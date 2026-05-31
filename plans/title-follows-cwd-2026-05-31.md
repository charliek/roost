# PR 1 plan — issue #196: model re-derives title from cwd

**Branch:** `fix/title-follows-cwd-model`
**Closes:** [#196](https://github.com/charliek/roost/issues/196)

## Problem

The tab title should follow the cwd on any shell, but today it only does so when the shell sends OSC 0 each prompt — i.e., when shell integration is loaded.

- Both Workspaces derive the title from cwd **only at `open_tab`** (Rust `state.rs:558` call inside `open_tab` which begins at `:549`; Swift `Workspace.swift:345` call inside `openTab` which begins at `:339`) via `derive_title` / `deriveTitle`.
- `setTabCwd` / `set_tab_cwd` (Mac `Workspace.swift:465`, Rust `state.rs:739`) updates only `cwd`; it does not re-derive title.
- `setTabTitleFromOSC` / `set_tab_title_from_osc` (Mac `Workspace.swift:454`, Rust `state.rs:716`) is the only ongoing title-update path. It depends on shell-integration's `__roost_title` emitting OSC 0 per prompt (`roost.zsh:65-68`, with a sister copy at the Mac path; both kept in sync per the file header).
- Shells without integration — notably **Apple `/bin/bash` 3.2** on macOS, which `bash_autobootstrap` skips at `pty.rs:543` because its ENV+POSIX path is SIP-patched, plus any explicit launcher like `bash --norc` — never send OSC 0, so the title stays frozen at the open-time leaf.

The cross-platform e2e `test_title_follows_cwd` is currently skipped on the `mac` target (`tools/roosttest/test_terminal.py:45-67`) because the Mac CI runner's default shell is Apple bash 3.2 → no OSC 0 → title doesn't follow cwd. Issue #196 was originally framed as Mac-specific; the corrected analysis is that this is a "shell-without-integration" gap on **both** UIs.

## Fix (Option A)

In both `set_tab_cwd` / `setTabCwd`, when `!user_titled` and the basename-derived title differs from the current title, also update the title and emit `TabTitleChanged` alongside `TabCwdChanged`. Mirrors the existing `user_titled` gate that `set_tab_title_from_osc` already uses. On integrated shells, OSC 0 from the next prompt overwrites the model's basename with the tilde-abbreviated path (`${PWD/#$HOME/~}`) within a prompt cycle.

### Why this option

- Symmetric across both UIs (preserves the one-core/two-impls parity contract).
- Small: ~10 lines per UI + 4 unit tests per UI + un-skip the e2e.
- Removes a hidden dependency on shell-integration for a feature that should be a model invariant.

### Trade-offs (honest list)

1. **Two `TabTitleChanged` events per `cd` on integrated shells.** Model emits basename; OSC 0 from the next prompt emits the tilde-abbreviated path (HOME-rooted form; e.g. `/Users/foo/projects/roost` → `~/projects/roost`, but `/usr` → `/usr` verbatim because `${PWD/#$HOME/~}` only substitutes when PWD starts with `$HOME`). Latest-wins; sub-100ms; no observable flicker. UI handlers fire twice per `cd` on integrated shells. **Note**: `events.subscribe` in IPC (`ipc.rs:899-910`, `IPCHandlerImpl.swift:134-143`) is currently `not-implemented`, so this stays in-process today; the doubled events will reach the wire when `events.subscribe` lands. Document the pairing in `docs/reference/ipc.md` (see step 7) so future consumers don't dedupe on the assumption of one-event-per-op.
2. **Brief format mismatch on integrated shells.** Model writes `usr`, OSC 0 then writes `/usr` (for `/usr`) or `~/projects/roost` (for HOME-rooted). Matters only to a test asserting exact equality on the title shortly after `set_tab_cwd`. None exist on the model path; `test_title_follows_cwd` does substring match; `test_title_follows_cd_via_script` (test_shell_integration.py:430) polls for equality with the OSC-0 form (`/usr`) so the transient basename is invisible to its `_wait` loop.
3. **CLI-supplied placeholder titles get refreshed on `cd`.** Already true via OSC 0 (the `open_tab` comment at Rust `:569-578` / Swift `:347-356` designates supplied titles as placeholders, `user_titled=false`). This widens the window from "every prompt on integrated shells" to "every `cd` everywhere", but it doesn't change the policy. Locking explicit titles is a separate decision (a different issue).
4. **`set_tab_cwd` gains a tiny extra responsibility.** Still one function, one guard, one extra `if`.
5. **Event order is `[TabCwdChanged, TabTitleChanged]`.** Cause-then-effect (cwd is the cause, title is the derived consequence). This is intentional — see step 8 below for the deliberate handling of the now-dead GTK fallback.

### Known cross-platform divergence to fix in this PR

`derive_title("/")` (Rust) returns `"shell"` (Path::file_name() is None for `/`). `deriveTitle(cwd: "/")` (Swift) returns `"/"` (`NSString.lastPathComponent` on `/` returns `/`). Until now this only mattered at `open_tab` time, which is rare for `cd /`. The model fix makes it routine. Both implementations should agree.

**Decision**: change Rust `derive_title("/")` to return `"/"` (mirror Swift, and mirror what OSC 0 would send via `${PWD/#$HOME/~}` for the root). This also fixes the UX gap where `cd /` on an un-integrated shell would persistently display `"shell"`. Update the existing Rust unit test if there is one.

### `user_titled` persistence: bundled into this PR

(Codex review caught this — was originally "out of scope", but deferring it introduces a regression.)

`user_titled` is currently not persisted in the state.json `TabSnapshot`. After relaunch, every tab returns with `user_titled=false`. Today this means:
- A manually-renamed tab "docs" survives restart on **un-integrated shells** (the title field IS persisted; only the lock is lost; without OSC 0 there's nothing to overwrite it).
- It loses the rename on the first OSC 0 on integrated shells (existing bug).

**Without `user_titled` persistence**, this PR's model fix would regress the un-integrated case: a restored "docs" tab loses its title on the FIRST `cd` everywhere (because `set_tab_cwd` would re-derive). This contradicts `docs/getting-started/keybindings.md:156` ("persisted and locked").

**Decision**: persist `user_titled` in this PR. Small mechanical change (3 fields + serde + restore wiring) — bundling it keeps the manual-rename invariant intact and matches the docs.

**Steps** (see change list step 0 below): add `user_titled: bool` to `TabSnapshot` in `store_json.rs`, plumb through `snapshot_for_persist`, take into account in `restore_from_layout` / `apply_restore_layout` on both UIs. Migration: missing field defaults to `false` (existing state.json files lose nothing; they didn't have the lock either).

## Change list

### 0. Persist `user_titled` across relaunch (prerequisite for step 1)

**Rust:**
- `crates/roost-linux/src/daemon/store_json.rs`: add `user_titled: bool` to `TabSnapshot` with `#[serde(default)]` so older state.json files load cleanly (missing field → `false`).
- `crates/roost-linux/src/daemon/state.rs`: in `snapshot_for_persist` (around `:1180`), populate `user_titled` from the `TabRow`. In `restore_from_layout` / `open_tab` restore path (around `:569-579`), accept the saved `user_titled` (don't hard-code to `false`).
- Verify `open_tab` callers in the restore path forward `user_titled`. Plumb through `restore_layout` / `apply_restore_layout` (`app.rs:980-999`).

**Swift:**
- `mac/Sources/Roost/Workspace.swift`: same change in the persisted snapshot struct (around `:782-786`), and in the restore path (around `:615-619`, `:347-365`).
- Mac's persistence struct keeps coding-keys explicit; add `userTitled: Bool = false` with a custom decoder that defaults to false on missing field (matching Codable conventions in-tree).

**Tests:**
- Rust: `user_titled_persists_across_relaunch` — open tab, `set_tab_title("manual")` (→ `user_titled=true`), persist, re-open workspace from disk, assert restored tab has `user_titled=true`. Then `set_tab_cwd("/usr")` and assert title stays `"manual"`.
- Swift: analog using `EventCapture` + the existing relaunch test pattern.
- Migration: a unit test that loads a state.json **without** the field and confirms `user_titled` defaults to `false` (backward compat).

### 1. `crates/roost-linux/src/daemon/state.rs::set_tab_cwd`

After mutating `row.cwd`, build the events vec to include a `TabTitleChanged` when `!row.user_titled` and the basename differs. Allocate `cwd_owned` once and reuse:

```rust
pub fn set_tab_cwd(&self, tab_id: i64, cwd: &str) -> Result<(), WorkspaceError> {
    let mut inner = self.inner.lock().unwrap();
    let row = inner
        .tabs
        .get_mut(&tab_id)
        .ok_or(WorkspaceError::TabNotFound(tab_id))?;
    let cwd_owned = cwd.to_string();
    row.cwd = cwd_owned.clone();
    let mut events = vec![WorkspaceEvent::TabCwdChanged {
        tab_id,
        cwd: cwd_owned,
    }];
    // Re-derive title from cwd when the user hasn't explicitly renamed.
    // Mirrors set_tab_title_from_osc's user_titled gate. Lets the title
    // follow cwd on shells without integration (Apple /bin/bash 3.2,
    // --norc bash); on integrated shells, the next prompt's OSC 0 refines
    // this to the tilde-abbreviated path. Order is cwd-then-title
    // (cause-then-effect); see the GTK Tab-N fallback note in step 8.
    if !row.user_titled {
        let new_title = derive_title(cwd);
        if row.title != new_title {
            row.title = new_title.clone();
            events.push(WorkspaceEvent::TabTitleChanged {
                tab_id,
                title: new_title,
            });
        }
    }
    self.commit(inner, events, Persist::Write);
    Ok(())
}
```

Also update `derive_title` to return `"/"` for the root case:

```rust
fn derive_title(cwd: &str) -> String {
    if cwd.is_empty() {
        return "shell".into();
    }
    if cwd == "/" {
        return "/".into();
    }
    std::path::Path::new(cwd)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "shell".into())
}
```

### 2. `mac/Sources/Roost/Workspace.swift::setTabCwd`

Same pattern, Swift idiom. Swift `deriveTitle("/")` already returns `"/"` (no change needed):

```swift
func setTabCwd(_ tabID: Int64, cwd: String) throws {
    guard var t = tabs[tabID] else { throw WorkspaceError.tabNotFound(tabID) }
    t.cwd = cwd
    var events: [Event] = [.tabCwdChanged(tabID: tabID, cwd: cwd)]
    // Re-derive title from cwd when the user hasn't explicitly renamed
    // (mirrors setTabTitleFromOSC's userTitled gate). On integrated
    // shells, OSC 0 from the next prompt refines this to the tilde-
    // abbreviated path. See the Rust twin for full rationale.
    if !t.userTitled {
        let newTitle = deriveTitle(cwd: cwd)
        if t.title != newTitle {
            t.title = newTitle
            events.append(.tabTitleChanged(tabID: tabID, title: newTitle))
        }
    }
    tabs[tabID] = t
    commit(events, persist: true)
}
```

### 3. Unit tests (Rust — `crates/roost-linux/src/daemon/state.rs::tests`)

Four tests, using the existing `ws.subscribe()` pattern:

- `set_tab_cwd_re_derives_title_when_not_user_titled` — open tab with `title=""` (→ derived to basename), `set_tab_cwd("/usr")` → title becomes `usr`; assert both `TabCwdChanged` (first) + `TabTitleChanged` (second) emitted, in that order.
- `set_tab_cwd_preserves_user_titled_title` — `set_tab_title("custom")` (→ `user_titled=true`), then `set_tab_cwd("/usr")` → title stays `custom`; only `TabCwdChanged` emitted.
- `set_tab_cwd_skips_title_event_when_basename_unchanged` — tab titled `tmp` from `/tmp`, `set_tab_cwd("/tmp")` (no-op `cd .`) → only `TabCwdChanged`, no `TabTitleChanged` churn.
- **`set_tab_cwd_overwrites_placeholder_title`** — open tab with `title="roostctl"` (explicit placeholder, `user_titled=false`), `set_tab_cwd("/usr")` → title becomes `usr`; assert `TabTitleChanged` emitted. Guards the regression class where a future refactor flips `open_tab` to `user_titled = !title.is_empty()`.

Also: extend the existing `derive_title` unit tests (if any — grep `derive_title` in `state.rs::tests`) to add `assert_eq!(derive_title("/"), "/")`.

### 4. Unit tests (Swift — `mac/Tests/RoostTests/WorkspaceStateTests.swift`)

Same four scenarios, using the in-tree `EventCapture` actor + `label(for:)` mapper at `WorkspaceStateTests.swift:148, 163` (the existing pattern for event-order assertions — handles Swift 6 strict Sendable correctly; do NOT hand-roll a closure-based collector).

Add a `deriveTitle("/")` regression in the Swift suite to lock the cross-platform agreement. Since `deriveTitle` is `private`, route the assertion through `openTab(... cwd: "/", title: "")` and assert the resulting `tab.title == "/"` (no visibility change needed).

### 5. Audit existing tests that call `set_tab_cwd` / `setTabCwd`

Verified call sites (grep before code lands):

- Rust `state.rs::cwd_changes_write_through` (state.rs:1554). Opens with `title=""` → derived to `start`, then `set_tab_cwd("/first")` derives `first`, then `set_tab_cwd("/second")` derives `second`. Only asserts on `restore.projects[0].tabs[0].cwd`. **No update needed** — test continues to pass; the saved snapshot now also carries the changed title, but the assertion is cwd-only.
- Rust `state.rs::flush_freezes_further_persistence` (similar shape). Same conclusion.
- Swift `WorkspaceStateTests::cwdChangesWriteThrough` (`WorkspaceStateTests.swift:289`) and `flushFreezesFurtherPersistence` (`:309`). Opens with `cwd: "/start", title: ""`. **No update needed** — same reasoning.

The previous draft of this plan misnamed the Rust test as `set_tab_cwd_writes_through` and the open-time title as `"only"` — both wrong; corrected here. Implementer: re-grep before claiming step 5 complete; if any new call sites appear, expect the extra `TabTitleChanged` only when the test inspects events directly.

### 6. Un-skip the e2e (`tools/roosttest/test_terminal.py:45-67`)

- Drop the `target` parameter from the test function signature (NOT the conftest fixture itself — `target` is a session-scoped fixture used transitively by `roost`).
- Remove the `if target == "mac": pytest.skip(...)` block.
- Replace the docstring to reflect the new mechanism. Suggested wording:

```python
def test_title_follows_cwd(roost, project):
    """The tab title follows the cwd via the model on any shell.

    Mechanism: set_tab_cwd re-derives title from cwd when !user_titled,
    emitting TabTitleChanged alongside TabCwdChanged. On shells with
    integration the next prompt's OSC 0 (__roost_title) refines the
    basename to the tilde-abbreviated full path (`${PWD/#$HOME/~}`) —
    latest-wins — but the model invariant holds regardless. Match the
    basename (`usr`): Mac shows the leaf (`usr`), GTK may show the path
    once OSC 0 fires (`usr` is in both). Poll, since events land a beat
    after `cd`.
    """
```

### 7. Documentation

Update `docs/reference/ipc.md` (around the event list at `:458-466`):

> **Note**: For a single `tab.set_cwd` (or OSC 7 routed through `apply_osc`), when `user_titled` is false the workspace may emit **both** `tab.cwd_changed` and `tab.title_changed` in that order. Consumers must treat the events as a pair, not assume one-event-per-op.

This is not optional; without it, future events.subscribe clients lock in a one-event-per-op contract by omission.

### 8. GTK `Tab N` fallback in `crates/roost-linux/src/app.rs:2104-2108`

The current handler overwrites the AdwTabPage title with `tilde_abbreviate(&cwd)` when current title `starts_with("Tab ")`. With the model fix, `set_tab_cwd` also emits `TabTitleChanged` (when `!user_titled`), so the fallback becomes redundant for the common case.

**Caveat (Codex caught this)**: The plan's earlier "Tab N never happens" rationale was overstated — `roostctl` and IPC clients can pass arbitrary titles including `"Tab 1"` (`crates/roost-cli/src/main.rs:248-258`, `roost-ipc::messages.rs:201-215`), and GTK's open/resync paths at `app.rs:1731` and `app.rs:2355` synthesize `"Tab {id}"` for tabs opened with empty titles. The fallback IS reachable.

**Decision**: drop the fallback anyway, because the model fix supersedes it (a tab with title `"Tab 1"` and `user_titled=false` will get re-titled on the first `set_tab_cwd`). For a tab opened with `"Tab 1"` that never gets a `cd`, the title stays `"Tab 1"` — same as without the fallback (the fallback only fires on `TabCwdChanged`, not on idle render). Net behavior is preserved for the no-cd case and improved for the cd case.

Document this in the PR body. If removal proves to interact with another path the e2e doesn't cover, fall back to leaving an `// XXX: superseded by model re-derivation in set_tab_cwd` comment instead. Per CLAUDE.md, no `// TODO:` left in committed code.

## Verification

- `cargo test -p roost-linux` (4 new model unit tests + 1 `user_titled` persistence test + 1 migration test + extended `derive_title` test + existing pass).
- `cd mac && swift test` (4 new model unit tests + 1 `userTitled` persistence test + 1 migration test + extended `deriveTitle("/")` regression + existing pass).
- `make e2e-gtk-ci` locally with `ROOST_TEST_MODE=1 --roost-fresh`. Cover:
  - `test_title_follows_cwd` (newly un-skipped) — model re-derives basename.
  - `test_title_follows_cd_via_script` — verifies the OSC 0 path still wins on integrated shells.
  - **NEW**: `test_manual_rename_survives_cd_and_relaunch` — open tab, `set_title("docs")`, `cd /usr`, assert title stays `docs`; close + reopen the workspace (the harness has `--roost-fresh` machinery but reopen across restart needs the e2e-style relaunch helper — if not available, the unit-test relaunch covers it and we skip the e2e variant).
- `cargo fmt --all -- --check` (CI lint gate; per `MEMORY.md`).
- `e2e-mac` job on the PR — primary verification of the Mac side, since I don't have a local Mac toolchain to spare.
- Manual on Mac (post-merge or via worktree): open a tab on Apple `/bin/bash`, `cd /usr`, confirm tab title becomes `usr`. Then `cd /`, confirm title becomes `/` (cross-platform parity). Then Cmd+R rename to `docs`, `cd /tmp`, confirm title stays `docs`. Restart, confirm `docs` persists; `cd /usr`, confirm title still `docs`.

### Pre-implementation grep checks

- Rust: `grep -rn 'derive_title' crates/roost-linux/` → verify no test asserts on `"shell"` for the root case, no IPC consumer treats `"shell"` as a tab title sentinel.
- Rust: in `state.rs::tests`, grep for `TabOpened` assertions that depend on `title="shell"` for `cwd="/"` opens. The base test at `state.rs:1223-1231` may emit `TabOpened` with title `"/"` instead of `"shell"` after the fix — verify expected.
- Swift: grep `deriveTitle` similarly.
- IPC: grep `\"/\"` in IPC handlers / cli for any title-as-sentinel use (Codex didn't find any; sanity-check again).

## Risks

- Step 5 audit: if any new call site appears in the grep, expect the extra `TabTitleChanged` only when the test inspects events. Mitigation: grep before writing impl.
- Step 8 removal of the GTK `Tab N` fallback: if any unobvious test depends on it, the e2e will catch it. Mitigation: GTK e2e covers tab-bar render via `tab.dump` text content.
- Cross-platform `derive_title("/")` change to `"/"`: verify no existing test asserts on `"shell"` for the root case. Grep `derive_title.*shell` before landing.

## Out of scope (acknowledged, filed separately)

- **Locking explicit CLI-supplied titles against OSC 0 / cwd refresh** (separate policy decision; a different issue). Today `open_tab(..., title="some-name")` still sets `user_titled=false` so the placeholder gets overwritten by OSC 0 / model derivation. Codex confirmed this is consistent with current policy.
- Changes to OSC 0 path or the auto-bootstrap (those are correct as-is).

(Note: `user_titled` persistence was originally out-of-scope; Codex's review caught it as a regression-class blocker, so it's now bundled into this PR — see step 0.)

## PR checklist

- [ ] **Step 0**: `user_titled` persisted in `TabSnapshot` (Rust + Swift) with `serde default = false`/Codable optional → backward-compat with old state.json. Restore path forwards the flag.
- [ ] Both UIs mirror-changed in `set_tab_cwd` / `setTabCwd`.
- [ ] Rust `derive_title("/")` returns `"/"` (cross-platform parity with Swift).
- [ ] 4 model-fix unit tests per UI (8 total) using the in-tree event-capture idioms (`ws.subscribe()` Rust; `EventCapture` actor Swift).
- [ ] 1 persistence test per UI ("manual title survives relaunch + cd").
- [ ] 1 migration test per UI (old state.json without `user_titled` field loads with default `false`).
- [ ] `derive_title("/") == "/"` Rust unit + Swift `openTab(cwd:"/")` regression.
- [ ] GTK `Tab N` fallback at `app.rs:2104-2108` dropped (or commented as superseded).
- [ ] `tools/roosttest/test_terminal.py::test_title_follows_cwd` un-skipped (signature drop only, fixture stays).
- [ ] Docstring rewritten to attribute to model re-derivation (not OSC 0).
- [ ] `docs/reference/ipc.md` notes the cwd/title event pairing.
- [ ] Audit-grep clean (no new call sites with event-shape assertions; no `"shell"` title sentinel use).
- [ ] `cargo fmt --all -- --check` green.
- [ ] PR body links #196, mentions the bundled `user_titled` persistence fix.
