"""pytest fixtures for the Roost E2E suite.

Parameterized by target UI (`--roost-target mac|gtk`, default `$ROOST_TARGET`
or `gtk`). A session fixture ensures the UI is running (launching it if
needed, and quitting only what it launched). Each test gets a fresh
`roost` client and a throwaway `project` that's cascade-cleaned after.
"""

from __future__ import annotations

import os
import uuid

import pytest
import ui
from client import Roost


# GLib domain `*-CRITICAL` lines that are known-benign under the headless e2e
# environment. Curate from a baseline run — currently EMPTY: the full GTK suite
# is critical-clean. Add entries as a tight substring + a `# reason`, mirroring
# the clippy.toml guards, only after confirming the line is environmental.
_GTK_CRITICAL_ALLOWLIST: list[str] = []


def pytest_addoption(parser):
    parser.addoption(
        "--roost-target", action="store", default=None, choices=list(ui.TARGETS),
        help="which UI to drive (mac|gtk); default $ROOST_TARGET or gtk",
    )
    parser.addoption(
        "--roost-fresh", action="store_true", default=False,
        help="force a harness-owned UI: quit any running instance and launch a "
             "hermetic one (seeded config + throwaway ROOST_STATE_DIR). Also via "
             "ROOST_TEST_FRESH=1. DESTRUCTIVE: closes a running dev session.",
    )


@pytest.fixture(scope="session")
def fresh(pytestconfig) -> bool:
    val = bool(pytestconfig.getoption("--roost-fresh")) or \
        os.environ.get("ROOST_TEST_FRESH") == "1"
    if val:
        # Export so non-fixture helpers (util.is_fresh) see flag-driven
        # fresh too, not just the env-driven form.
        os.environ["ROOST_TEST_FRESH"] = "1"
    return val


@pytest.fixture(scope="session")
def target(pytestconfig) -> str:
    return (
        pytestconfig.getoption("--roost-target")
        or os.environ.get("ROOST_TARGET")
        or "gtk"
    )


@pytest.fixture(scope="session", autouse=True)
def _ui_session(target, fresh):
    # `start_session` returns True when the harness owns the instance
    # (launched it with a throwaway state dir); only then do we quit it +
    # clean up at teardown. A reused dev UI is left running and untouched.
    started_here = ui.start_session(target, fresh=fresh)
    # Capture the captured-log path up front: `end_session` clears the global,
    # but the file persists, so the #234 critical gate below can read it after
    # the UI has fully stopped (complete log, including shutdown-time lines).
    gtk_log = ui._GTK_LOG if (started_here and target == "gtk") else None
    yield
    if started_here:
        ui.end_session(target)
    # Gate (#234): with the harness-owned GTK UI now stopped, fail the session
    # if it emitted any non-allowlisted GLib `*-CRITICAL`. The focus crash was a
    # `Gtk-CRITICAL` storm no individual test would fail on (a critical doesn't
    # fail a test on its own); this is the cheap permanent net the panel asked
    # for. Folded into this fixture — rather than a separate one — so the scan
    # runs deterministically AFTER `end_session`, not on fragile cross-fixture
    # teardown order. Surfaces as an ERROR-at-teardown (still a non-zero exit).
    # (`G_DEBUG=fatal-criticals` as a separate diagnostic lane is a follow-up.)
    if gtk_log is not None and gtk_log.exists():
        bad = [
            line
            for line in gtk_log.read_text(errors="replace").splitlines()
            if "-CRITICAL **:" in line
            and not any(allow in line for allow in _GTK_CRITICAL_ALLOWLIST)
        ]
        assert not bad, (
            f"{len(bad)} non-allowlisted GLib *-CRITICAL line(s) in the GTK UI "
            "log — a focus/teardown regression like #234 surfaces here. "
            "First 20:\n" + "\n".join(bad[:20])
        )


@pytest.fixture
def roost(target):
    client = Roost(ui.socket_path(target))
    try:
        yield client
    finally:
        client.close()


@pytest.fixture
def project(roost):
    """A throwaway project for one test; cascade-cleaned afterward."""
    pid = roost.create_project(name=f"pytest-{uuid.uuid4().hex[:8]}", cwd="/tmp")
    try:
        yield pid
    finally:
        try:
            roost.delete_project(pid)
        except Exception:
            pass  # a test may have already cascade-closed it


@pytest.fixture
def palette(roost):
    """Drive the palette from a known-closed state, and leave it closed.

    The palette is global UI state (one at a time), so a leaked-open
    palette from a failed test would wedge the next one. Dismiss on both
    sides (idempotent — a no-op when already closed)."""
    roost.palette_dismiss()
    yield roost
    roost.palette_dismiss()


def pytest_terminal_summary(terminalreporter, exitstatus, config):
    """Make skips loud: print every skipped test + reason at session end.

    Skips are how the suite hid regressions both ways (a test mode left off
    locally; a feature unimplemented behind a CI skip). Surfacing the count
    + reasons means a run that quietly skipped half the suite can't read as
    "all green" — the reviewer sees `SKIPS: N` and what was dropped.
    """
    skipped = terminalreporter.stats.get("skipped", [])
    if not skipped:
        return
    terminalreporter.write_sep("-", f"SKIPS: {len(skipped)}")
    for rep in skipped:
        reason = ""
        lr = getattr(rep, "longrepr", None)
        # A skip's longrepr is the (path, lineno, "Skipped: <reason>") tuple.
        if isinstance(lr, tuple) and len(lr) == 3:
            reason = str(lr[2]).removeprefix("Skipped: ")
        terminalreporter.write_line(f"  SKIP {rep.nodeid} — {reason}")
