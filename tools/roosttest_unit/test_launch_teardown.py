"""Plan 070 D7: the harness never leaves a UI it launched behind when the
boot wait fails.

The iced path runs real stub executables, because what is pinned is that a
live process dies. Each test also records the stub's own `Popen` and kills it
in cleanup, so a bug in the code under test cannot leak a process. The Mac
path is `open`-launched, so it is pinned with mocks.
"""

from __future__ import annotations

import os
import stat
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

ROOSTTEST_DIR = Path(__file__).resolve().parents[1] / "roosttest"
sys.path.insert(0, str(ROOSTTEST_DIR))

import ui  # noqa: E402

# `ui.launch` passes the binary no arguments, so the readiness path rides
# in the environment.
_STUB_READY_ENV = "ROOST_TEST_STUB_READY"

# Ignores the first SIGTERM and exits on the second, like a UI wedged
# before its event loop.
_DOUBLE_TERM_STUB = """#!/bin/bash
trap_count=0
handle_term() {
  trap_count=$((trap_count + 1))
  if [ "$trap_count" -ge 2 ]; then
    exit 0
  fi
}
trap handle_term TERM
echo $$ > "$ROOST_TEST_STUB_READY"
while true; do
  sleep 0.2
done
"""

# Ignores SIGTERM entirely; only SIGKILL ends it.
_IGNORE_TERM_STUB = """#!/bin/bash
trap '' TERM
echo $$ > "$ROOST_TEST_STUB_READY"
while true; do
  sleep 0.2
done
"""

_REAL_WAIT_ALIVE = ui.wait_alive


def _fast_wait_alive(target: str, timeout: float = 5.0) -> None:
    """The boot wait with the stub's trap known to be installed, and a 5 s
    (scaled) budget instead of 30 s."""
    _wait_for_pid_file(Path(os.environ[_STUB_READY_ENV]), ui.scaled_timeout(timeout))
    _REAL_WAIT_ALIVE(target, timeout=timeout)


def _write_stub(path: Path, body: str) -> Path:
    path.write_text(body)
    mode = path.stat().st_mode
    path.chmod(mode | stat.S_IEXEC | stat.S_IXGRP | stat.S_IXOTH)
    return path


def _wait_for_pid_file(path: Path, timeout: float = 10.0) -> int:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if path.exists():
            content = path.read_text().strip()
            if content:
                return int(content)
        time.sleep(0.05)
    raise AssertionError(f"stub never wrote its readiness file at {path}")


def _pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def _kill_and_reap(proc: subprocess.Popen) -> None:
    if proc.poll() is None:
        proc.kill()
    proc.wait(timeout=10)


class _IsolatedIcedLaunchTestCase(unittest.TestCase):

    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory(prefix="roost-stop-owned-ui-", dir="/tmp")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.state_dir = self.root / "state"
        self.state_dir.mkdir()
        self.runtime_dir = self.root / "run"
        self.runtime_dir.mkdir()
        self.ready_file = self.root / "ready"

        env_patch = mock.patch.dict(
            os.environ,
            {
                "XDG_RUNTIME_DIR": str(self.runtime_dir),
                "ROOST_E2E_LOG_DIR": str(self.root / "logs"),
                _STUB_READY_ENV: str(self.ready_file),
            },
        )
        env_patch.start()
        self.addCleanup(env_patch.stop)
        os.environ.pop("ROOST_ICED_APP", None)
        # `socket_path` ignores XDG_RUNTIME_DIR on macOS; pin it here so no
        # platform can dial a real UI.
        socket_patch = mock.patch("ui.socket_path", lambda _target: self.runtime_dir / "roost.sock")
        socket_patch.start()
        self.addCleanup(socket_patch.stop)

        saved_globals = {
            name: getattr(ui, name)
            for name in (
                "_SESSION_STATE_DIR",
                "_ICED_PROC",
                "_ICED_LOG",
                "_ICED_BUNDLE_LOG_OFFSET",
                "_ICED_BUNDLE_PID",
            )
        }
        self.addCleanup(self._restore_globals, saved_globals)

        self.assertFalse(ui.is_alive("iced"))

    def _restore_globals(self, saved: dict) -> None:
        for name, value in saved.items():
            setattr(ui, name, value)

    def _stub(self, body: str) -> Path:
        return _write_stub(self.root / "stub-roost-iced", body)

    def _launch_expecting_failure(self, stub: Path) -> int:
        spawned: list[subprocess.Popen] = []
        real_popen = subprocess.Popen

        def recording_popen(*args, **kwargs):
            proc = real_popen(*args, **kwargs)
            spawned.append(proc)
            self.addCleanup(_kill_and_reap, proc)
            return proc

        with (
            mock.patch.dict(os.environ, {"ROOST_ICED_BIN": str(stub)}),
            mock.patch("ui._SESSION_STATE_DIR", self.state_dir),
            mock.patch("ui.wait_alive", _fast_wait_alive),
            mock.patch("ui.subprocess.Popen", recording_popen),
        ):
            with self.assertRaises(Exception):
                ui.launch("iced")
        self.assertEqual(len(spawned), 1, "launch() should spawn exactly the stub")
        return spawned[0].pid


class StopOwnedUiKillsTheStubTests(_IsolatedIcedLaunchTestCase):
    def test_a_stub_that_ignores_one_sigterm_is_dead_after_launch_raises(self) -> None:
        stub = self._stub(_DOUBLE_TERM_STUB)
        pid = self._launch_expecting_failure(stub)
        self.assertFalse(
            _pid_alive(pid),
            "launch() raised but the double-SIGTERM stub is still running",
        )

    def test_a_stub_that_ignores_sigterm_entirely_is_dead_after_launch_raises(self) -> None:
        stub = self._stub(_IGNORE_TERM_STUB)
        pid = self._launch_expecting_failure(stub)
        self.assertFalse(
            _pid_alive(pid),
            "launch() raised but the SIGTERM-ignoring stub is still running "
            "(SIGKILL should have ended it)",
        )


class MacLaunchStopsTheAppTests(unittest.TestCase):
    def setUp(self) -> None:
        self.addCleanup(mock.patch.stopall)
        saved_offset = ui._MAC_LOG_OFFSET
        self.addCleanup(setattr, ui, "_MAC_LOG_OFFSET", saved_offset)

    def test_both_attempts_timing_out_stops_the_app_before_reraising(self) -> None:
        with (
            mock.patch("ui._mac_cleanup"),
            mock.patch("ui.subprocess.run"),
            mock.patch("ui.wait_alive", side_effect=TimeoutError("did not boot")),
            mock.patch("ui._quit_mac_process") as quit_mac,
        ):
            with self.assertRaises(TimeoutError):
                ui._launch_mac(Path("/Applications/Roost.app"))
        quit_mac.assert_called_once_with()

    def test_any_other_boot_error_stops_the_app_before_reraising(self) -> None:
        with (
            mock.patch("ui._mac_cleanup"),
            mock.patch("ui.subprocess.run"),
            mock.patch("ui.wait_alive", side_effect=RuntimeError("refused")),
            mock.patch("ui._quit_mac_process") as quit_mac,
        ):
            with self.assertRaises(RuntimeError):
                ui._launch_mac(Path("/Applications/Roost.app"))
        quit_mac.assert_called_once_with()

    def test_a_retry_that_succeeds_never_stops_the_app(self) -> None:
        with (
            mock.patch("ui._mac_cleanup"),
            mock.patch("ui.subprocess.run"),
            mock.patch("ui.wait_alive", side_effect=[TimeoutError("did not boot"), None]),
            mock.patch("ui._quit_mac_process") as quit_mac,
        ):
            ui._launch_mac(Path("/Applications/Roost.app"))  # must not raise
        quit_mac.assert_not_called()


if __name__ == "__main__":
    unittest.main()
