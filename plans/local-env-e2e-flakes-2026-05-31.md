# PR 3 plan — local-env e2e flakes (test_osc52 + test_sidebar_layout)

**Branch:** `fix/local-env-e2e-flakes`

Two flakes surface when running `make e2e-gtk-ci` on macOS (the GTK dev build), confirmed pre-existing on `main` (don't reproduce on real Linux CI). Both have clear root causes; neither is a UI bug.

## Flake 1 — `test_osc52_writes_selection_clipboard` on macOS GTK

### Root cause

`_seed_baseline(roost, "selection")` does `clipboard_write("selection", baseline)` followed by `clipboard_dump("selection")` and asserts equality. On GTK:
- `clipboard::write(Target::Primary, ...)` (`crates/roost-linux/src/clipboard.rs:26-30`) is `#[cfg(target_os = "linux")]`-gated to no-op off Linux.
- `ipc_clipboard_dump(Primary)` (`app.rs:3677-3681`) returns `Ok(None)` off Linux.

So on macOS GTK dev: write is a no-op, dump returns `None`, `_seed_baseline` asserts `None == 'baseline-XXX'` → failure.

The matching `test_osc52_writes_system_clipboard` passes (uses `Target::Clipboard` which IS wired on macOS GTK via the standard `display.clipboard()`). On `--roost-target mac` the same selection test PASSES (Mac UI uses a named NSPasteboard). The failure is **macOS-GTK-dev-build-only**.

### Fix

Skip `test_osc52_writes_selection_clipboard` on macOS GTK dev build, matching the existing in-tree pattern for "GTK-Linux-only path" (precedents at `test_shell_integration.py:113` and `:141`).

**Imports** (module top of `test_osc52.py`): add `import pytest` (currently absent — `pytest.skip(...)` would NameError without it) and `import sys` (currently absent — sibling test_shell_integration.py imports it at module top, not inline).

```python
def test_osc52_writes_selection_clipboard(roost, project, target):
    if target == "gtk" and sys.platform == "darwin":
        pytest.skip(
            "GTK selection clipboard (X11/Wayland PRIMARY) is Linux-only; "
            "macOS GTK dev build has no PRIMARY. System clipboard covered "
            "by test_osc52_writes_system_clipboard. Real GTK on Linux runs "
            "this in e2e-gtk CI."
        )
    ...
```

The skip is precise: only the `selection` variant, only on the dev profile. The `system` variant and the `read_request` variant keep running on macOS GTK (they use the wired `Target::Clipboard`).

## Flake 2 — `test_sidebar_holds_width_on_window_resize` race in full suite

### Root cause

Passes in isolation; fails after `test_sidebar_collapse_persistence`. Failure: `sidebar_width 0.0 out of [160, 400]` while `sidebar_collapsed == False`.

`test_collapsed_sidebar_survives_project_delete` ends with `_toggle_to_visible(roost)` (sidebar flipped from collapsed to visible), then `fresh.close()`. GTK's `set_visible(true)` queues a layout pass on the idle; until that runs, `sidebar_box.width() == 0` even though `is_visible() == true`. The next test's `_resize_settle(roost, 1100)` waits only on `window_width` to reach the target — sidebar's relayout hasn't run yet, so `sidebar_width` is captured as 0.

`ipc_window_metrics` confirms the metric shape (`app.rs:3889-3899`): `collapsed = !sidebar_box.is_visible()`, `sw = sidebar_box.width()` (only when not collapsed). So `(collapsed=false, sidebar_width=0)` is a real transient state immediately after `set_visible(true)`.

On CI, `test_sidebar_collapse_persistence` is `skip_on_ci`-gated entirely (the quit+relaunch lifecycle isn't reliable under bare xvfb / slow LaunchServices), so it never toggles the sidebar — the layout test sees a clean fresh sidebar and passes. Hence the bug is local-only, not a CI issue.

### Fix

Tighten `_resize_settle` to wait for the sidebar to also reach a steady state when not collapsed. Two simplify findings drive the final shape:

- **Drop `_wait_window_width`**: it becomes dead code after the refactor (its only consumer was `_resize_settle`). Keep one source of `WIDTH_TOLERANCE_PT` truth.
- **Don't conflate the two timeout reasons**: a layout-stall (WM granted resize but sidebar stayed at 0) should be distinguishable from a WM-refused-resize. Surface both metric fields in the eventual failure assertion via the existing dump, and add a tiny diagnostic to the bounds assertion so a regression isn't mistaken for the original flake.

```python
def _resize_settle(roost, target_width: float) -> dict:
    """Request `target_width` and return the settled metrics. Does NOT
    fail if the WM refuses or only partially grants the resize — the
    caller gates on the achieved delta.

    Settling is BOTH window width AND sidebar layout: GTK's `set_visible`
    queues a relayout on the idle, so a sidebar that was just flipped to
    visible can momentarily report `is_visible=true` with `width=0` until
    the next layout pass runs. Waiting on the window only would race that
    interval.

    On timeout (WM refused, OR sidebar never settled), returns whatever
    metrics are currently visible. The caller's `achieved < USABLE_DELTA_PT`
    skip covers the WM-refused case; the bounds assertion catches the
    layout-stall case with a metric snapshot so the two failure modes
    don't read identically.
    """
    roost.window_resize(target_width, 700)
    try:
        Roost._wait(
            lambda: _window_and_sidebar_settled(roost, target_width),
            timeout=2.0,
            what=f"window+sidebar settle to {target_width}",
        )
    except Timeout:
        pass  # WM may have refused; or sidebar layout stalled — caller assertions disambiguate
    return roost.window_metrics()


def _window_and_sidebar_settled(roost, target_width: float) -> bool:
    """Predicate: window width within tolerance AND, when the sidebar
    is visible, its width is non-zero (i.e., the GTK layout pass has
    run after set_visible(true))."""
    m = roost.window_metrics()
    if abs(m["window_width"] - target_width) > WIDTH_TOLERANCE_PT:
        return False
    if not m["sidebar_collapsed"] and m["sidebar_width"] <= 0:
        return False
    return True
```

Then update the existing bounds assertion in `test_sidebar_holds_width_on_window_resize` so a layout-stall doesn't read as the original flake:

```python
assert 160 <= baseline_sidebar <= 400, (
    f"sidebar starting width {baseline_sidebar} out of [160, 400] "
    f"(collapsed={before['sidebar_collapsed']}, "
    f"window_width={before['window_width']}). "
    "A width of 0.0 with collapsed=False indicates a layout-stall — "
    "the GTK set_visible(true) idle relayout didn't run within the "
    "_resize_settle budget; check for a preceding test that toggled "
    "the sidebar without waiting for settle."
)
```

**Also fix the silent-false-green in the shrink test** (simplify finding #4 + Codex 2nd pass): `test_sidebar_holds_width_on_window_shrink` lacks both `assert not before['sidebar_collapsed']` AND `assert 160 <= baseline_sidebar <= 400`. Adding only the collapsed-check isn't enough — Codex pointed out that with `collapsed=False` but `sidebar_width==0` (the real bug we're fixing), baseline=0 → after-shrink=0 → `|0-0|<=1pt` trivially holds. Add BOTH:

```python
# After the achieved-delta gate:
assert not before["sidebar_collapsed"], "sidebar must be visible for the test"
assert 160 <= baseline_sidebar <= 400, (
    f"sidebar starting width {baseline_sidebar} out of [160, 400] "
    f"(collapsed={before['sidebar_collapsed']}, "
    f"window_width={before['window_width']}). "
    "0.0 with collapsed=False indicates a layout-stall — see "
    "test_sidebar_holds_width_on_window_resize for the diagnostic."
)
```

**Fix the root cause in `_toggle_to_visible`** (Codex 2nd pass): The fundamental defect is in `tools/roosttest/test_sidebar_collapse_persistence.py::_toggle_to_visible` — it asserts `sidebar_collapsed == False` but doesn't wait for the GTK layout pass to materialize a non-zero width. That's the source of the leak; downstream tests then inherit `visible=True, width=0`. Update the helper so the cleanup boundary itself waits for the settle:

```python
def _toggle_to_visible(roost: Roost) -> None:
    """Drive the palette to restore the sidebar. No-op if already
    visible. After toggle, wait for the GTK layout pass to materialize
    a non-zero width — otherwise the next test inherits a
    `visible=True, width=0` transient (GTK runs the layout on an idle
    cycle after `set_visible(true)`)."""
    metrics = roost.window_metrics()
    if metrics["sidebar_collapsed"]:
        roost.palette_open()
        roost.palette_query("toggle sidebar")
        roost.palette_activate("toggle_sidebar")
    # Always wait for settle — the sibling `_toggle_to_collapsed` path
    # also flows through here on retest cycles.
    Roost._wait(
        lambda: _sidebar_visible_and_allocated(roost),
        timeout=2.0,
        what="sidebar to settle visible with non-zero width",
    )


def _sidebar_visible_and_allocated(roost: Roost) -> bool:
    m = roost.window_metrics()
    return not m["sidebar_collapsed"] and m["sidebar_width"] > 0
```

Both fixes ship together: the cleanup-wait is the root-cause fix; the `_resize_settle` predicate is belt-and-suspenders so any future test that starts with `_resize_settle` is robust to any preceding state. Codex framed this as "the bounded-width check inside `_toggle_to_visible` OR an autouse fixture" — going with the helper update because it's smaller and matches the existing palette-drive idiom in the file.

Net diff: ~40 LOC (factor `_window_and_sidebar_settled`, drop `_wait_window_width`, update `_resize_settle`, expand the bounds-assertion diagnostic, add the shrink-test guards, update `_toggle_to_visible` + add `_sidebar_visible_and_allocated`). The capability-gate logic (achieved-delta `>= USABLE_DELTA_PT`) keeps working unchanged.

**Upstream / UI-side fix** (Codex 2nd pass): Considered fixing the underlying GTK race inside `toggle_sidebar`'s IPC path (the palette dispatches the toggle synchronously but the layout pass is queued on the idle). Codex correctly pointed out this would be too broad for a test-isolation PR and would need test-mode settle semantics across both UIs. Defer — the standard GTK allocation semantics aren't a UI bug, just a test-harness gap. No upstream report needed without a minimal repro.

## Verification

- `ROOST_TEST_MODE=1 uv run --group test pytest tools/roosttest/test_osc52.py --roost-target gtk --roost-fresh -v` → 2 passed, 1 skipped (was: 1 failed).
- `ROOST_TEST_MODE=1 uv run --group test pytest tools/roosttest/test_sidebar_collapse_persistence.py tools/roosttest/test_sidebar_layout.py --roost-target gtk --roost-fresh -v` → all pass (was: layout-test failure).
- `make e2e-gtk-ci` → 0 failures locally (was: 2 unrelated failures). SKIPS summary gains exactly one new entry (the macOS-GTK-PRIMARY skip); existing skips unchanged.
- Real CI on GTK runner: `test_osc52_writes_selection_clipboard` still RUNS (not skipped on Linux, since `sys.platform == "linux"`), so coverage of the PRIMARY path stays.
- `cargo fmt --all -- --check` not applicable (pure Python).

## Out of scope

- Wiring NSPasteboard "selection" backing into the macOS GTK dev build's clipboard.rs (would add real coverage but the dev build is dev-only; the Mac target has the path).
- Restructuring the sidebar-collapse-persistence tests' cleanup (they currently rely on `_toggle_to_visible` at end; the race-fix in `_resize_settle` is more general).

## PR checklist

- [ ] `test_osc52.py` — add module-top `import pytest` and `import sys` (currently absent). Add macOS-GTK-dev skip to `test_osc52_writes_selection_clipboard` only (with `target` fixture param + clear reason).
- [ ] `test_sidebar_layout.py` — factor out `_window_and_sidebar_settled` predicate; `_resize_settle` uses it; drop the now-orphaned `_wait_window_width` helper.
- [ ] Expand the bounds-assertion error message in `test_sidebar_holds_width_on_window_resize` so a layout-stall doesn't read as the original flake (include `collapsed`, `window_width`, and a hint about preceding test toggles).
- [ ] Add BOTH `assert not before["sidebar_collapsed"]` AND `assert 160 <= baseline_sidebar <= 400` (with the layout-stall diagnostic) to `test_sidebar_holds_width_on_window_shrink`. The collapsed-check alone leaves the `visible=True, width=0` false-green that Codex flagged.
- [ ] Update `_toggle_to_visible` in `test_sidebar_collapse_persistence.py` to wait for `sidebar_collapsed=False AND sidebar_width > 0` via a `_sidebar_visible_and_allocated` predicate — fixes the root cause (cleanup boundary doesn't wait for GTK layout pass).
- [ ] Local `make e2e-gtk-ci` → 0 failures. SKIPS summary gains exactly one new entry (the macOS-GTK-PRIMARY skip); count the actual N before/after rather than predicting it.
- [ ] No change to the existing CI gates (both tests still gated by their existing checks on real Linux).
