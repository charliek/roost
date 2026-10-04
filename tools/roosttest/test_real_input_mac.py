"""Real CGEvent and Accessibility input against a branch Roost-Iced.app (plan 074 §D7).

The iced UI's test ops (`app.key_event`, `app.context_menu_*`) start after
winit and AppKit have had their say; this module starts before them. Its
events are real: posted by `tools/input/mac/roost-input-mac` through
`tools/input/mac/runner.py` (runner mode through the TCC anchor app on the
harness Mac, direct mode on CI).

**It owns its UI** (`owns_ui`), so conftest's shared launcher never runs for
it. Each test launches the branch's `mac/build/Roost-Iced.app` (or
`ROOST_ICED_APP`) with `open -n` under `ROOST_BUNDLE_PROFILE=linux`: the
`Roost-linux` namespace, apart from an installed Roost-Iced.app and its
`Roost-iced` socket. Each launch gets its own `ROOST_STATE_DIR`,
`ROOST_TEST_MODE=1` and a config seeded with `fixtures/launcher.conf`'s safety
lines (`local-backend = in-process`, so no `roost-session` starts;
`agent-hooks = off`, so nothing is written into a real agent's dotfiles) plus
the test's own keys. The branch build shares the installed app's bundle id, so
the launched app is handled by **pid** only, and only once its environment
proves it is this launch's (`real_input_ui`): frontmost by pid, quit by pid
with SIGTERM — never by bundle id, `osascript` or process name.

`Roost-linux` is one fixed namespace, not a sandbox: a run holds
`/tmp/roost-real-input.lock`, refuses to start while that namespace's socket
answers, creates the namespace's directories itself (each with this run's
marker file), proves the launched pid owns the socket, and copies
`~/Library/Logs/Roost-linux` into the artifacts before cleaning up what it
created, still marked, after owning the namespace throughout, and while holding
the namespace's socket lock: the logs directory goes; the caches directory and
its `roost.lock` stay.
Nothing here asserts on what the profile changes (the app id, the window
title).

`make e2e-iced-real-input-mac` runs it with `ROOST_REQUIRE_REAL_INPUT=1`, under
which unavailable real input is a failure rather than a skip.
"""

from __future__ import annotations

import json
import os
import platform
import re
import sys
import tempfile
from pathlib import Path

import pytest
import ui
import util
from real_input_ui import Namespace, RealInputUI, platform_mismatch

sys.path.insert(0, str(ui.REPO_ROOT / "tools" / "input" / "mac"))
import runner as real_input  # noqa: E402

pytestmark = pytest.mark.owns_ui

KEY_A = 0  # kVK_ANSI_A


@pytest.fixture(scope="module", autouse=True)
def _macos_iced_only(target):
    if (reason := platform_mismatch(platform.system(), target)) is not None:
        real_input.skip_or_fail(reason)


@pytest.fixture(scope="module")
def artifacts() -> Path:
    base = os.environ.get("ROOST_E2E_ARTIFACT_DIR")
    path = Path(base) if base else Path(tempfile.mkdtemp(prefix="roost-real-input-"))
    path = path.expanduser().resolve()
    path.mkdir(parents=True, exist_ok=True)
    return path


@pytest.fixture(scope="module")
def namespace(artifacts):
    """Hold `Roost-linux` for the module (see `real_input_ui.Namespace`)."""
    held = Namespace(artifacts)
    if (reason := held.acquire()) is not None:
        real_input.skip_or_fail(reason)
    try:
        yield held
    finally:
        held.close()


@pytest.fixture(scope="module")
def helper(namespace, artifacts):
    """The helper, once a preflight says this machine can do real input now
    (`runner.readiness`): every test sits behind that gate."""
    app = _bundle_app()
    with real_input.unavailable_skips():
        handle = real_input.Helper(artifacts)
    try:
        with real_input.unavailable_skips():
            report = handle.require_ready()
        (artifacts / "run.json").write_text(
            json.dumps(
                {"mode": handle.mode, "helper": str(handle.binary), "app": str(app), "preflight": report}
            )
        )
        yield handle
    finally:
        handle.close()


def _bundle_app() -> Path:
    app = ui.iced_bundle_app() or ui.REPO_ROOT / "mac" / "build" / "Roost-Iced.app"
    if not (app / "Contents" / "MacOS" / ui.ICED_BUNDLE_EXECUTABLE_NAME).is_file():
        pytest.fail(
            f"no Roost-Iced bundle at {app}: assemble it with mac/scripts/bundle-iced.sh debug "
            "(make e2e-iced-real-input-mac does)"
        )
    return app.resolve()


@pytest.fixture
def launch_ui(namespace):
    """Launch a fresh UI with this test's config keys (`launch_ui({"macos-
    option-as-alt": "left"})`); `relaunch()` restarts one on its own state,
    and every UI launched here is quit, and its state removed, at the end."""
    launched: list[RealInputUI] = []

    def launch(config: dict[str, str] | None = None) -> RealInputUI:
        launched.append(RealInputUI(_bundle_app(), config or {}, namespace))
        return launched[-1].launch()

    try:
        yield launch
    finally:
        errors = []
        for running in reversed(launched):
            try:
                running.quit()
            except Exception as error:  # quit them all, then report the first
                errors.append(error)
        if errors:
            raise errors[0]


def test_preflight(helper, launch_ui):
    """Behind the `helper` fixture's readiness gate (the read-only grant
    checks, an unlocked console, a US input source), one real operation: a
    real key posted into Roost comes back out of `tab.capture_pty_input`."""
    with real_input.unavailable_skips():
        roost = launch_ui()
        tab = roost.open_tab(["/bin/cat"])
        roost.bring_to_front(helper)
        util.drain(roost.client, tab)
        helper.key(roost.pid, KEY_A)
        got = util.drain_until_match(roost.client, tab, re.escape(b"a"))
        assert got + util.drain(roost.client, tab) == b"a"
