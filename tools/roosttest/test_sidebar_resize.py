"""Sidebar resize contract — `sidebar.set_width` on all three UIs.

The programmatic twin of dragging the seam (plan 011). One op, three
arms (Iced, GTK, Mac), one behavioral contract pinned here:

* the applied width is observable on `app.window_metrics` and reaches
  the terminal grid (a wider sidebar means fewer PTY columns);
* out-of-band widths **clamp** to `[160, 400]` rather than erroring;
* non-positive widths are rejected with `invalid-param`;
* a set while collapsed persists silently and lands on expand;
* a non-default width survives a window resize (the sidebar-hold
  invariant of `test_sidebar_layout.py`, re-asserted off the default);
* and it survives a quit → relaunch (the persistence half).

`sidebar.set_width` is test-mode-gated, so the whole file is skipped
when `ROOST_TEST_MODE` is unset — the bare attempt would fail with
`not-enabled` and produce noisy "wrong reason" failures. Same
convention as `test_sidebar_layout.py`.

Tolerance: 1pt, matching `test_sidebar_layout.py`. Divider thickness
and HiDPI rounding shift the measured pane width sub-point; every
assertion here is against a >= 20pt intended change.

Non-finite widths (`nan`, `inf`) are rejected by the same server-side
guard as `0`/`-50`, but JSON has no literal for them, so they can't be
driven from this harness — `crates/roost-engine` and the Swift handler
cover that arm in unit tests.
"""

from __future__ import annotations

import os

import pytest

import ui
from client import Roost, RoostError, Timeout
from test_sidebar_collapse_persistence import _toggle_to_collapsed, _toggle_to_visible
from util import BARE_SHELL_ARGV, skip_on_ci, wait_tab_attached


TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"
WIDTH_TOLERANCE_PT = 1.0
# Achieved window-width delta we need before the hold-invariant assertion
# is meaningful (the bug would grow the sidebar well beyond 1pt over this).
USABLE_DELTA_PT = 200.0

# The clamp band, verbatim from `SIDEBAR_MIN_WIDTH`/`SIDEBAR_MAX_WIDTH`
# (crates/roost-engine/src/workspace.rs) and `sidebarMinWidth`/
# `sidebarMaxWidth` (mac/Sources/Roost/App.swift).
MIN_WIDTH_PT = 160.0
MAX_WIDTH_PT = 400.0

# The baseline every test seeds before it mutates: comfortably inside the
# band, and far enough from BASELINE + 80 that the column comparison in
# `test_set_width_reflects_in_metrics_and_pty_cols` cannot be swallowed by
# cell-width quantization.
BASELINE_WIDTH_PT = 220.0
WIDER_WIDTH_PT = 300.0


def _sidebar_width_is(roost: Roost, expected: float) -> bool:
    """Predicate: the sidebar is expanded AND reports `expected` ±1pt.

    Both clauses matter. GTK's `set_visible(true)` flips `is_visible()`
    synchronously but queues the layout pass on the idle cycle, so a
    freshly-uncollapsed sidebar reports `collapsed=False, width=0` for
    an interval — see `test_sidebar_layout._window_and_sidebar_settled`
    for the same race.
    """
    m = roost.window_metrics()
    if m["sidebar_collapsed"]:
        return False
    return abs(m["sidebar_width"] - expected) <= WIDTH_TOLERANCE_PT


def _wait_sidebar_width(roost: Roost, expected: float, timeout: float = 5.0) -> None:
    """Poll `app.window_metrics` until the sidebar settles at `expected`.

    The op replies once the UI has applied it, but the *layout* that
    materializes the new allocation runs on the toolkit's next pass on
    both Rust UIs, so the read back is genuinely asynchronous. On
    overrun, surface the last metrics — "stalled at the old width" and
    "clamped somewhere else" must not read identically.
    """
    try:
        Roost._wait(
            lambda: _sidebar_width_is(roost, expected),
            timeout=timeout,
            what=f"sidebar width to settle at {expected}",
        )
    except Timeout as exc:
        raise AssertionError(
            f"sidebar never reached {expected}±{WIDTH_TOLERANCE_PT}pt; "
            f"last metrics {roost.window_metrics()}"
        ) from exc


def _seed_baseline(roost: Roost) -> None:
    """Every test starts from the same explicit state — expanded, at
    `BASELINE_WIDTH_PT` — rather than inheriting whatever the previous
    test (or the attached developer UI) left behind."""
    _toggle_to_visible(roost)
    roost.sidebar_set_width(BASELINE_WIDTH_PT)
    _wait_sidebar_width(roost, BASELINE_WIDTH_PT)


def _settled_cols(roost: Roost, tab: int, width: float, timeout: float = 5.0) -> int:
    """The tab's column count once its grid has stopped moving under a
    `width`pt sidebar.

    A single `tab.dump` right after attach can still report the
    renderer's seed grid: the terminal is created before the first real
    layout pass quantizes it against the pane, so an immediate read can
    record a baseline that was never the 220pt layout — and the later
    "strictly fewer columns" comparison would then be against a number
    the sidebar never produced. Require the same count across two
    *poll iterations* (>= one 100ms interval apart, not two back-to-back
    reads) with the sidebar confirmed at `width` throughout.
    """
    seen: dict[str, int | None] = {"last": None}

    def stable() -> bool:
        if not _sidebar_width_is(roost, width):
            seen["last"] = None  # a width in flight invalidates the run
            return False
        cols = roost.dump(tab)["cols"]
        if cols > 0 and cols == seen["last"]:
            return True
        seen["last"] = cols
        return False

    try:
        Roost._wait(
            stable,
            timeout=timeout,
            what=f"tab {tab} grid to settle under a {width}pt sidebar",
        )
    except Timeout as exc:
        raise AssertionError(
            f"tab {tab} grid never settled under a {width:.0f}pt sidebar; "
            f"last cols={seen['last']}, metrics {roost.window_metrics()}"
        ) from exc
    assert seen["last"] is not None
    return seen["last"]


def _live_tab(roost: Roost, project: int) -> int:
    """An attached, focused tab whose grid tracks the terminal pane.

    Focused deliberately: `tab.dump` reads the tab's live grid, and a
    tab in a non-active project isn't guaranteed to be re-gridded by
    every adapter's layout pass. A bare shell keeps startup output (and
    OSC 133 marks) out of the picture — the grid is all this needs.
    """
    tab = roost.open_tab(project, cwd="/tmp", argv=BARE_SHELL_ARGV)
    wait_tab_attached(roost, tab)
    roost.focus(tab)
    return tab


def _window_and_sidebar_settled(roost: Roost, target_width: float) -> bool:
    m = roost.window_metrics()
    if abs(m["window_width"] - target_width) > WIDTH_TOLERANCE_PT:
        return False
    if not m["sidebar_collapsed"] and m["sidebar_width"] <= 0:
        return False
    return True


def _resize_settle(roost: Roost, target_width: float) -> dict:
    """Request `target_width` and return the settled metrics. Does NOT
    fail if the WM refuses, only partially grants, or stalls the resize —
    the caller gates on the achieved delta. Verbatim in spirit from
    `test_sidebar_layout._resize_settle`; see that file for why a refused
    resize must never fail a test.
    """
    roost.window_resize(target_width, 700)
    try:
        Roost._wait(
            lambda: _window_and_sidebar_settled(roost, target_width),
            timeout=2.0,
            what=f"window+sidebar settle to {target_width}",
        )
    except Timeout:
        pass  # WM refused, OR sidebar layout stalled — disambiguated downstream
    return roost.window_metrics()


@pytest.fixture(scope="module", autouse=True)
def _restore_sidebar(target):
    """Capture the sidebar state this module inherited and put it back.

    The harness may be attached to an already-running developer UI, so
    nothing here may hardcode a "known good" width or collapse state on
    the way out. Its own client (the `roost` fixture is function-scoped,
    and `test_width_survives_relaunch` invalidates any client that
    outlives its quit → relaunch), opened fresh on both ends.

    A collapsed sidebar reports `sidebar_width: 0.0` — the stored
    expand-target is not observable over IPC — so capture *expands*
    first, reads the real width, and re-collapses. Without that round
    trip the recorded width would be 0 (not a settable value), the
    tests below would overwrite the hidden expand-target, and teardown
    would restore only the collapse — leaking the last test's 300pt
    into a reused developer UI the next time they hit ⌘B.
    """
    with Roost(ui.socket_path(target)) as client:
        original_collapsed = bool(client.window_metrics()["sidebar_collapsed"])
        # `_toggle_to_visible` waits for a non-zero allocation, so the
        # read below can't catch the GTK `visible=True, width=0` transient.
        _toggle_to_visible(client)
        original_width = float(client.window_metrics()["sidebar_width"])
        if original_collapsed:
            _toggle_to_collapsed(client)

    yield

    with Roost(ui.socket_path(target)) as client:
        # Expand first either way: the width has to be applied against a
        # laid-out pane for the metrics poll to confirm it landed.
        _toggle_to_visible(client)
        client.sidebar_set_width(original_width)
        _wait_sidebar_width(client, original_width)
        if original_collapsed:
            _toggle_to_collapsed(client)


@pytest.mark.skipif(
    not TEST_MODE,
    reason="sidebar.set_width requires ROOST_TEST_MODE=1 in the UI's launch env",
)
class TestSidebarResize:
    def test_set_width_reflects_in_metrics_and_pty_cols(self, roost, project):
        """The applied width is observable, and it reaches the PTY.

        Metrics alone would pass on a UI that moved the divider but never
        re-gridded the terminal, which is the regression that matters to
        a user: a wider sidebar MUST take columns away from the shell.
        """
        _seed_baseline(roost)
        tab = _live_tab(roost, project)
        baseline_cols = _settled_cols(roost, tab, BASELINE_WIDTH_PT)

        roost.sidebar_set_width(WIDER_WIDTH_PT)
        _wait_sidebar_width(roost, WIDER_WIDTH_PT)

        # The PTY resize is async on every adapter (layout pass → grid
        # quantization → TIOCSWINSZ), so poll rather than read once.
        try:
            Roost._wait(
                lambda: roost.dump(tab)["cols"] < baseline_cols,
                timeout=5.0,
                what=f"tab {tab} to lose columns to the wider sidebar",
            )
        except Timeout as exc:
            raise AssertionError(
                f"widening the sidebar {BASELINE_WIDTH_PT:.0f}→{WIDER_WIDTH_PT:.0f}pt "
                f"did not shrink the terminal grid: cols still "
                f"{roost.dump(tab)['cols']} (was {baseline_cols}); "
                f"metrics {roost.window_metrics()}"
            ) from exc

    def test_set_width_clamps_to_band(self, roost):
        """Out-of-band widths land on the nearest bound, not an error.

        The clamp lives in the workspace (Rust) / the bridge (Swift), so
        both ends of the band are asserted through the same op the seam
        drag routes through — a UI that clamped only in its own widget
        would persist an out-of-band value and restore it on relaunch.
        """
        _seed_baseline(roost)

        roost.sidebar_set_width(90)
        _wait_sidebar_width(roost, MIN_WIDTH_PT)

        roost.sidebar_set_width(1000)
        _wait_sidebar_width(roost, MAX_WIDTH_PT)

    def test_set_width_rejects_invalid(self, roost):
        """Zero and negative widths are rejected before reaching the UI.

        Clamping them into the band would silently turn a caller's bug
        into a 160pt sidebar; the op draws the line at "not a width".
        """
        _seed_baseline(roost)

        for bad in (0, -50):
            with pytest.raises(RoostError) as exc:
                roost.sidebar_set_width(bad)
            assert exc.value.code == "invalid-param", (
                f"sidebar.set_width({bad}) must be rejected as invalid-param, "
                f"got {exc.value.code}: {exc.value.message}"
            )
        # The rejected calls must not have moved anything.
        assert _sidebar_width_is(roost, BASELINE_WIDTH_PT), (
            f"a rejected width changed the sidebar: {roost.window_metrics()}"
        )

    def test_set_width_while_collapsed_applies_on_expand(self, roost):
        """Setting a width while collapsed stays invisible until expand.

        The op is the drag's twin, and a drag can't happen without a
        seam — so the collapsed case is defined as "persist it, reveal it
        on expand" rather than "expand implicitly".
        """
        _seed_baseline(roost)
        _toggle_to_collapsed(roost)

        roost.sidebar_set_width(350)
        # No wait needed: `call` is a synchronous round-trip and the UI
        # applies the op before replying, so this read is ordered after
        # it. A width that leaked out here would be visible immediately.
        collapsed_metrics = roost.window_metrics()
        assert collapsed_metrics["sidebar_collapsed"], (
            f"sidebar.set_width must not expand a collapsed sidebar; "
            f"got {collapsed_metrics}"
        )
        # `< 1` rather than `== 0`: the Rust UIs report a literal 0.0 while
        # collapsed, but the Mac UI reports the pane's real frame width and
        # *defines* collapsed as `< 1pt`. Either way, nothing of the 350
        # just set may be visible.
        assert collapsed_metrics["sidebar_width"] < 1.0, (
            f"a collapsed sidebar must not reveal the stored width; "
            f"got {collapsed_metrics}"
        )

        _toggle_to_visible(roost)
        _wait_sidebar_width(roost, 350)

    def test_sidebar_holds_nondefault_width_on_window_resize(self, roost):
        """The `test_sidebar_layout.py` hold-invariant, off the default.

        That file pins the invariant at whatever width the UI launched
        with; a UI that hardcoded its launch width into the layout pass
        would pass it and still snap a user-chosen width back on the
        next window resize. This seeds a non-default width first.
        """
        _seed_baseline(roost)
        roost.sidebar_set_width(WIDER_WIDTH_PT)
        _wait_sidebar_width(roost, WIDER_WIDTH_PT)

        try:
            # Capability gate BEFORE any geometry assertion: under a
            # constraining WM (xvfb, tiling compositors) the window size
            # is unreliable, so skip first if the WM won't grant a usable
            # resize. Never fail via a resize timeout.
            before = _resize_settle(roost, 1100)
            after = _resize_settle(roost, 1800)
            achieved = abs(after["window_width"] - before["window_width"])
            if achieved < USABLE_DELTA_PT:
                pytest.skip(
                    f"WM granted only a {achieved:.0f}pt window delta "
                    f"({before['window_width']:.0f}→{after['window_width']:.0f}); "
                    "need ≥200pt to exercise the sidebar-hold invariant (no WM?)"
                )
            assert abs(after["sidebar_width"] - WIDER_WIDTH_PT) <= WIDTH_TOLERANCE_PT, (
                f"a user-chosen {WIDER_WIDTH_PT:.0f}pt sidebar must survive a "
                f"{achieved:.0f}pt window widen; got {after['sidebar_width']} "
                f"(metrics {after})"
            )
        finally:
            # Leave the window where `test_sidebar_layout.py` leaves it
            # rather than at 1800 — later files in the session read
            # geometry too.
            _resize_settle(roost, 1100)

    def test_width_survives_relaunch(self, roost, target):
        """The persistence half: a chosen width outlives the process.

        GTK/Iced write it through to `state.json` (`sidebar_width`), the
        Mac UI to `RoostSidebarWidth` in UserDefaults. Both must restore
        it at launch — a UI that only persisted on drag-release, or that
        overwrote the stored value with its launch default, fails here.
        """
        # Ownership gate, BEFORE anything is mutated. `quit + launch` is
        # destructive and only meaningful against an instance the harness
        # started: attached to a developer's own UI it would close their
        # session and relaunch a differently-configured one (on Mac the
        # relaunch always passes ROOST_DEFAULTS_SUITE, so the width would
        # be read back out of the isolated test suite it was never written
        # to — a spurious persistence failure on top of the destruction).
        # `owned_session_config_path()` is None exactly when the harness
        # reused a running UI instead of launching one.
        if ui.owned_session_config_path() is None:
            pytest.skip(
                "quit + relaunch would close a developer's own UI and relaunch a "
                "differently-configured one; requires a harness-owned instance "
                "(--roost-fresh / ROOST_TEST_FRESH=1, or no UI already running)"
            )
        skip_on_ci(
            "quit + relaunch is unreliable on CI: GTK xvfb has no WM, and the slow "
            "macOS LaunchServices respawn pushes wait_alive past its 90s budget",
            alt_coverage="Rust sidebar_width_persists_across_reopen + "
            "Swift sidebarWidthPersistsClampedValue",
        )
        _seed_baseline(roost)
        roost.sidebar_set_width(WIDER_WIDTH_PT)
        _wait_sidebar_width(roost, WIDER_WIDTH_PT)

        # The old client cannot outlive the process it is connected to.
        roost.close()
        ui.quit(target)
        ui.launch(target)

        fresh = Roost(ui.socket_path(target))
        try:
            _wait_sidebar_width(fresh, WIDER_WIDTH_PT)
        finally:
            fresh.close()
