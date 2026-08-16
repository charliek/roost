"""Unit coverage for the `ROOST_ICED_APP` bundle-launch path in
`tools/roosttest/ui.py` (plan 027 C4/W5).

No real launches here — `linux-test`/`e2e-iced-bundle` cover the live path.
This pins: launch-mode selection (bundle vs bare-binary Popen), the three
loud validation errors, and the log-offset/pid bookkeeping the bundle path
uses in place of `_ICED_PROC`.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

ROOSTTEST_DIR = Path(__file__).resolve().parents[1] / "roosttest"
sys.path.insert(0, str(ROOSTTEST_DIR))

import ui  # noqa: E402
from client import RoostError  # noqa: E402


def _make_bundle(root: Path, executable_name: str = "Roost-Iced") -> Path:
    app = root / "Roost-Iced.app"
    macos_dir = app / "Contents" / "MacOS"
    macos_dir.mkdir(parents=True)
    (macos_dir / executable_name).write_text("#!/bin/sh\n")
    return app


class IcedBundleAppValidationTests(unittest.TestCase):
    """`iced_bundle_app()` validates eagerly, at the point ROOST_ICED_APP is
    read, and never falls back silently to the bare binary."""

    def test_unset_returns_none(self) -> None:
        with patch.dict(os.environ, {}, clear=False):
            os.environ.pop("ROOST_ICED_APP", None)
            self.assertIsNone(ui.iced_bundle_app())

    @patch("ui.platform.system", return_value="Linux")
    def test_set_off_darwin_raises_loudly(self, _system) -> None:
        with patch.dict(os.environ, {"ROOST_ICED_APP": "/anywhere.app"}):
            with self.assertRaisesRegex(RuntimeError, "macOS-only"):
                ui.iced_bundle_app()

    @patch("ui.platform.system", return_value="Darwin")
    def test_missing_app_path_raises_loudly(self, _system) -> None:
        with tempfile.TemporaryDirectory() as root:
            missing = Path(root) / "Roost-Iced.app"
            with patch.dict(os.environ, {"ROOST_ICED_APP": str(missing)}):
                with self.assertRaisesRegex(FileNotFoundError, "does not exist"):
                    ui.iced_bundle_app()

    @patch("ui.platform.system", return_value="Darwin")
    def test_missing_executable_raises_loudly_not_a_silent_fallback(self, _system) -> None:
        with tempfile.TemporaryDirectory() as root:
            app = Path(root) / "Roost-Iced.app"
            (app / "Contents" / "MacOS").mkdir(parents=True)
            # Deliberately no Roost-Iced executable inside.
            with patch.dict(os.environ, {"ROOST_ICED_APP": str(app)}):
                with self.assertRaisesRegex(FileNotFoundError, "missing its executable"):
                    ui.iced_bundle_app()

    @patch("ui.platform.system", return_value="Darwin")
    def test_valid_bundle_resolves_the_app_path(self, _system) -> None:
        with tempfile.TemporaryDirectory() as root:
            app = _make_bundle(Path(root))
            with patch.dict(os.environ, {"ROOST_ICED_APP": str(app)}):
                self.assertEqual(ui.iced_bundle_app(), app)

    @patch("ui.platform.system", return_value="Darwin")
    def test_relative_path_resolves_against_repo_root(self, _system) -> None:
        # Mirrors `rust_binary_path`'s override convention (ROOST_<TARGET>_BIN):
        # a relative override is repo-root-relative, not cwd-relative.
        with tempfile.TemporaryDirectory() as root:
            _make_bundle(Path(root) / "mac" / "build")
            with patch("ui.REPO_ROOT", Path(root)):
                with patch.dict(
                    os.environ, {"ROOST_ICED_APP": "mac/build/Roost-Iced.app"}
                ):
                    self.assertEqual(
                        ui.iced_bundle_app(),
                        Path(root) / "mac" / "build" / "Roost-Iced.app",
                    )


class LaunchModeSelectionTests(unittest.TestCase):
    """`launch("iced", ...)` picks the bundle path only when `ROOST_ICED_APP`
    resolves to something; otherwise the Popen path is untouched."""

    def test_launch_dispatches_to_bundle_helper_when_app_resolves(self) -> None:
        bundle = Path("/Applications/Roost-Iced.app")
        with (
            patch("ui.is_alive", return_value=False),
            patch("ui._SESSION_STATE_DIR", None),
            patch("ui.iced_bundle_app", return_value=bundle),
            patch("ui._launch_iced_bundle") as launch_bundle,
            patch("ui.subprocess.run") as run,
        ):
            ui.launch("iced")
            launch_bundle.assert_called_once_with(bundle, state_dir=None)
        run.assert_not_called()

    def test_launch_falls_through_to_popen_path_when_app_unset(self) -> None:
        state_dir = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, state_dir, True)
        with (
            patch("ui.is_alive", return_value=False),
            patch("ui.iced_bundle_app", return_value=None),
            patch("ui._launch_iced_bundle") as launch_bundle,
            patch("ui.rust_binary_path", return_value=(Path("/usr/bin/true"), True)),
            patch("ui._SESSION_STATE_DIR", state_dir),
            patch("ui._session_config_path", return_value=state_dir / "launcher.conf"),
            patch("ui.wait_alive"),
            patch("ui.subprocess.Popen") as popen,
        ):
            popen.return_value = Mock(pid=999)
            ui.launch("iced", state_dir=state_dir)
            launch_bundle.assert_not_called()
        popen.assert_called_once()

    def test_launch_clears_a_stale_bundle_log_offset_on_the_popen_path(self) -> None:
        """A prior bundle-mode launch in this process must not leak into a
        later bare-binary launch's `_launch_output`/`_boot_refusal`."""
        state_dir = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, state_dir, True)
        with (
            patch("ui.is_alive", return_value=False),
            patch("ui.iced_bundle_app", return_value=None),
            patch("ui._ICED_BUNDLE_LOG_OFFSET", 123),
            patch("ui.rust_binary_path", return_value=(Path("/usr/bin/true"), True)),
            patch("ui._SESSION_STATE_DIR", state_dir),
            patch("ui._session_config_path", return_value=state_dir / "launcher.conf"),
            patch("ui.wait_alive"),
            patch("ui.subprocess.Popen") as popen,
        ):
            popen.return_value = Mock(pid=999)
            ui.launch("iced", state_dir=state_dir)
            self.assertIsNone(ui._ICED_BUNDLE_LOG_OFFSET)

    def test_launch_clears_a_stale_bundle_pid_on_the_popen_path(self) -> None:
        """A prior bundle-mode launch's *pid* must not survive into a later
        bare-binary launch either — otherwise `quit("iced")` would dispatch
        to bundle teardown (signalling a long-dead pid) instead of
        terminating the live bare-binary process this launch just started."""
        state_dir = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, state_dir, True)
        with (
            patch("ui.is_alive", return_value=False),
            patch("ui.iced_bundle_app", return_value=None),
            patch("ui._ICED_BUNDLE_PID", 4242),
            patch("ui.rust_binary_path", return_value=(Path("/usr/bin/true"), True)),
            patch("ui._SESSION_STATE_DIR", state_dir),
            patch("ui._session_config_path", return_value=state_dir / "launcher.conf"),
            patch("ui.wait_alive"),
            patch("ui.subprocess.Popen") as popen,
        ):
            popen.return_value = Mock(pid=999)
            ui.launch("iced", state_dir=state_dir)
            self.assertIsNone(ui._ICED_BUNDLE_PID)


class LogOffsetBookkeepingTests(unittest.TestCase):
    """`_launch_output("iced")` falls back to the bundle's persistent log,
    offset-scoped, exactly like the mac branch does for `_MAC_LOG_OFFSET`."""

    def _log(self, text: str) -> Path:
        root = Path(tempfile.mkdtemp(prefix="roost-unit-iced-bundle-log-"))
        self.addCleanup(shutil.rmtree, root, True)
        log = root / "roost.log"
        log.write_text(text)
        return log

    def test_no_offset_and_no_proc_is_empty(self) -> None:
        with (
            patch("ui._ICED_PROC", None),
            patch("ui._ICED_BUNDLE_LOG_OFFSET", None),
        ):
            self.assertEqual(ui._launch_output("iced"), "")

    def test_bundle_offset_reads_the_bundle_log_from_the_recorded_offset(self) -> None:
        log = self._log("stale\nresolved bundle identity bundle_id=\"x\"\n")
        offset = len("stale\n")
        with (
            patch("ui._ICED_BUNDLE_LOG_OFFSET", offset),
            patch("ui._iced_bundle_ui_log_path", return_value=log),
        ):
            out = ui._launch_output("iced")
        self.assertNotIn("stale", out)
        self.assertIn("resolved bundle identity", out)

    def test_popen_path_still_wins_when_bundle_offset_is_none(self) -> None:
        """Bundle-mode fallback must not shadow the ordinary Popen capture
        path when the harness launched iced the normal way."""
        log = self._log("popen output\n")
        proc = Mock()
        with (
            patch("ui._ICED_PROC", proc),
            patch("ui._ICED_LOG", log),
            patch("ui._ICED_BUNDLE_LOG_OFFSET", None),
        ):
            self.assertIn("popen output", ui._launch_output("iced"))


class BundleIdentityLogAssertionTests(unittest.TestCase):
    def test_missing_log_line_raises(self) -> None:
        with patch("ui._launch_output", return_value="boot line only\n"):
            with self.assertRaisesRegex(RuntimeError, "did not log the W3"):
                ui._assert_bundle_identity_logged()

    def test_present_log_line_passes(self) -> None:
        line = (
            'INFO resolved bundle identity bundle_id="ai.stridelabs.Roost.iced" '
            'profile="iced"\n'
        )
        with patch("ui._launch_output", return_value=line):
            ui._assert_bundle_identity_logged()  # must not raise


class TestModeCanaryTests(unittest.TestCase):
    """Cleanup is `delete_project` only, never an explicit `close_tab` first:
    closing a project's only tab cascades to delete the project (plan 026
    D8), so closing it explicitly would race the harness's own
    `delete_project` call against that cascade."""

    def test_not_enabled_is_reported_as_a_dropped_test_mode(self) -> None:
        client = Mock()
        client.identify.return_value = {"pid": 4242}
        client.create_project.return_value = 1
        client.open_tab.return_value = 2
        client.tab_feed_pty_bytes.side_effect = RoostError("not-enabled", "nope")
        with patch("ui.Roost", return_value=client):
            with self.assertRaisesRegex(RuntimeError, "env forwarding dropped"):
                ui._assert_test_mode_canary("iced")
        client.close_tab.assert_not_called()
        client.delete_project.assert_called_once_with(1)
        client.close.assert_called_once()

    def test_other_errors_propagate_unwrapped(self) -> None:
        client = Mock()
        client.identify.return_value = {"pid": 4242}
        client.create_project.return_value = 1
        client.open_tab.return_value = 2
        client.tab_feed_pty_bytes.side_effect = RoostError("not-found", "gone")
        with patch("ui.Roost", return_value=client):
            with self.assertRaisesRegex(RoostError, "not-found"):
                ui._assert_test_mode_canary("iced")
        client.delete_project.assert_called_once_with(1)

    def test_delete_project_not_found_is_swallowed_as_an_already_cascaded_delete(
        self,
    ) -> None:
        client = Mock()
        client.identify.return_value = {"pid": 4242}
        client.create_project.return_value = 1
        client.open_tab.return_value = 2
        client.delete_project.side_effect = RoostError("not-found", "gone")
        with patch("ui.Roost", return_value=client):
            ui._assert_test_mode_canary("iced")  # must not raise
        client.close.assert_called_once()

    def test_success_cleans_up_the_throwaway_project_without_closing_the_tab_first(
        self,
    ) -> None:
        client = Mock()
        client.identify.return_value = {"pid": 4242}
        client.create_project.return_value = 1
        client.open_tab.return_value = 2
        with patch("ui.Roost", return_value=client):
            ui._assert_test_mode_canary("iced")
        client.tab_feed_pty_bytes.assert_called_once_with(2, b"")
        client.close_tab.assert_not_called()
        client.delete_project.assert_called_once_with(1)


class BundlePidVerificationTests(unittest.TestCase):
    """`_launch_iced_bundle` refuses to adopt a pid that doesn't belong to
    the bundle's own process before recording it for teardown."""

    def test_mismatched_process_name_refuses_to_adopt_the_pid(self) -> None:
        with (
            patch("ui._iced_bundle_ui_log_path", return_value=Path(tempfile.mktemp())),
            patch("ui.subprocess.run"),
            patch("ui.wait_alive"),
            patch("ui._answering_pid", return_value=4242),
            patch("ui._process_command", return_value="/usr/bin/something-else"),
        ):
            with self.assertRaisesRegex(RuntimeError, "refusing to adopt"):
                ui._launch_iced_bundle(Path("/Applications/Roost-Iced.app"))
        self.assertIsNone(ui._ICED_BUNDLE_PID)

    def test_matching_process_name_adopts_the_pid(self) -> None:
        # Set the allowlisted vars (plus ROOST_BUNDLE_PROFILE, which must
        # NOT be forwarded) so the assertions below exercise real content,
        # not mocks asserting on mocks. ROOST_TEST_MODE=1 means the real
        # gate would fire the live canary, so it's mocked below too.
        with (
            patch.dict(
                os.environ,
                {
                    "ROOST_TEST_MODE": "1",
                    "RUST_LOG": "debug",
                    "ROOST_BUNDLE_PROFILE": "mac",
                },
                clear=True,
            ),
            patch("ui._iced_bundle_ui_log_path", return_value=Path(tempfile.mktemp())),
            patch("ui.subprocess.run") as run,
            patch("ui.wait_alive"),
            patch("ui._answering_pid", return_value=4242),
            patch("ui._process_command", return_value="/Applications/Roost-Iced.app/Contents/MacOS/Roost-Iced"),
            patch("ui._assert_bundle_identity_logged") as assert_identity,
            patch("ui._assert_test_mode_canary") as assert_canary,
        ):
            try:
                ui._launch_iced_bundle(Path("/Applications/Roost-Iced.app"))
                self.assertEqual(ui._ICED_BUNDLE_PID, 4242)
            finally:
                ui._ICED_BUNDLE_PID = None
        open_argv = run.call_args_list[0].args[0]
        self.assertEqual(open_argv[0], "open")
        self.assertIn("ROOST_TEST_MODE=1", open_argv)
        self.assertIn("RUST_LOG=debug", open_argv)
        self.assertFalse(
            any(arg.startswith("ROOST_BUNDLE_PROFILE=") for arg in open_argv),
            f"ROOST_BUNDLE_PROFILE must never be forwarded into the bundle "
            f"launch (it exists to exercise the bundle-id-derived default "
            f"profile): {open_argv!r}",
        )
        assert_identity.assert_called_once()
        assert_canary.assert_called_once_with("iced")

    def test_executable_outside_the_bundle_refuses_to_adopt_the_pid(self) -> None:
        """Same executable *name* as the bundle, but not living inside it —
        e.g. a `Roost-Iced` on $PATH from an unrelated build. Must not be
        adopted even though the process-name check alone would pass."""
        with (
            patch("ui._iced_bundle_ui_log_path", return_value=Path(tempfile.mktemp())),
            patch("ui.subprocess.run"),
            patch("ui.wait_alive"),
            patch("ui._answering_pid", return_value=4242),
            patch("ui._process_command", return_value="/usr/local/bin/Roost-Iced"),
        ):
            with self.assertRaisesRegex(RuntimeError, "not under the launched bundle"):
                ui._launch_iced_bundle(Path("/Applications/Roost-Iced.app"))
        self.assertIsNone(ui._ICED_BUNDLE_PID)


class LaunchFailureTeardownTests(unittest.TestCase):
    """`_launch_iced_bundle` must not leave a launched bundle running behind
    any exception raised after the `open` spawn — else a boot-validation
    failure (or a canary failure) leaks a live Roost-Iced process into
    whatever the harness runs next."""

    def test_identity_mismatch_pkills_by_name_when_no_pid_was_ever_adopted(
        self,
    ) -> None:
        with (
            patch("ui._iced_bundle_ui_log_path", return_value=Path(tempfile.mktemp())),
            patch("ui.subprocess.run") as run,
            patch("ui.wait_alive"),
            patch("ui._answering_pid", return_value=4242),
            patch("ui._process_command", return_value="/usr/bin/something-else"),
        ):
            with self.assertRaisesRegex(RuntimeError, "refusing to adopt"):
                ui._launch_iced_bundle(Path("/Applications/Roost-Iced.app"))
        self.assertIsNone(ui._ICED_BUNDLE_PID)
        pkill_calls = [c.args[0] for c in run.call_args_list if c.args[0][0] == "pkill"]
        self.assertEqual(pkill_calls, [["pkill", "-x", ui.ICED_BUNDLE_EXECUTABLE_NAME]])

    def test_canary_failure_after_pid_adoption_pid_kills_the_bundle(self) -> None:
        """Once identity is confirmed and the pid is recorded, a later
        failure (here: the ROOST_TEST_MODE canary) must tear the bundle
        down through the pid-based path, not a name-based `pkill`."""
        with (
            patch.dict(os.environ, {"ROOST_TEST_MODE": "1"}, clear=True),
            patch("ui._iced_bundle_ui_log_path", return_value=Path(tempfile.mktemp())),
            patch("ui.subprocess.run") as run,
            patch("ui.wait_alive"),
            patch("ui._answering_pid", return_value=4242),
            patch(
                "ui._process_command",
                return_value="/Applications/Roost-Iced.app/Contents/MacOS/Roost-Iced",
            ),
            patch("ui._assert_bundle_identity_logged"),
            patch(
                "ui._assert_test_mode_canary",
                side_effect=RuntimeError("env forwarding dropped ROOST_TEST_MODE"),
            ),
            patch("ui._pid_alive", return_value=False),  # short-circuits to "already dead"
        ):
            with self.assertRaisesRegex(RuntimeError, "env forwarding dropped"):
                ui._launch_iced_bundle(Path("/Applications/Roost-Iced.app"))
        # The pid-based path (`_quit_iced_bundle`), not a name kill.
        pkill_calls = [c.args[0] for c in run.call_args_list if c.args[0][0] == "pkill"]
        self.assertEqual(pkill_calls, [])
        self.assertIsNone(ui._ICED_BUNDLE_PID)


class BootLivenessDispatchTests(unittest.TestCase):
    """`_boot_refusal` must use the read-only bundle-name probe while a
    bundle launch is in flight (no pid recorded yet), not the generic
    `_ICED_PROC.poll()` branch (which is always None in bundle mode and
    would otherwise permanently read as "still booting")."""

    def _log(self, text: str) -> Path:
        root = Path(tempfile.mkdtemp(prefix="roost-unit-iced-bundle-refusal-"))
        self.addCleanup(shutil.rmtree, root, True)
        log = root / "roost.log"
        log.write_text(text)
        return log

    def test_running_bundle_process_is_not_a_refusal(self) -> None:
        with (
            patch("ui._ICED_BUNDLE_LOG_OFFSET", 0),
            patch("ui._roost_iced_bundle_running", return_value=True),
        ):
            self.assertIsNone(ui._boot_refusal("iced"))

    def test_exited_bundle_process_with_refusal_line_is_reported(self) -> None:
        log = self._log(
            "Error: another Roost (pid 1) is using this state directory; "
            "exiting rather than writing state.json from two processes.\n"
        )
        with (
            patch("ui._ICED_BUNDLE_LOG_OFFSET", 0),
            patch("ui._iced_bundle_ui_log_path", return_value=log),
            patch("ui._roost_iced_bundle_running", return_value=False),
        ):
            message = ui._boot_refusal("iced")
        self.assertIsNotNone(message)
        assert message is not None
        self.assertIn("refusal, not a hang", message)


class QuitDispatchTests(unittest.TestCase):
    """`quit("iced")` must route to the pid-based bundle teardown when the
    harness owns a bundle-launched process, and never touch `Roost` by name."""

    def test_quit_dispatches_to_bundle_teardown_when_a_pid_is_recorded(self) -> None:
        with (
            patch("ui._ICED_BUNDLE_PID", 4242),
            patch("ui._quit_iced_bundle") as quit_bundle,
        ):
            ui.quit("iced")
        quit_bundle.assert_called_once()

    def test_quit_is_a_pure_no_op_when_no_bundle_pid_was_recorded(self) -> None:
        with (
            patch("ui._ICED_BUNDLE_PID", None),
            patch("ui.subprocess.run") as run,
        ):
            ui._quit_iced_bundle()
        run.assert_not_called()

    def test_quit_escalates_to_sigkill_and_proves_death_before_returning(self) -> None:
        with (
            patch("ui._ICED_BUNDLE_PID", 4242),
            patch("ui._pid_alive", return_value=True),  # the one up-front liveness check
            patch(
                "ui._process_command",
                return_value="/Applications/Roost-Iced.app/Contents/MacOS/Roost-Iced",
            ),
            patch("ui._wait_pid_gone", side_effect=[False, True]),  # SIGTERM window times out, SIGKILL window doesn't
            patch("ui.subprocess.run") as run,
        ):
            ui._quit_iced_bundle()
        self.assertEqual(
            [call.args[0] for call in run.call_args_list],
            [["kill", "4242"], ["kill", "-9", "4242"]],
        )
        self.assertIsNone(ui._ICED_BUNDLE_PID)

    def test_quit_raises_rather_than_returning_when_sigkill_is_survived(self) -> None:
        with (
            patch("ui._ICED_BUNDLE_PID", 4242),
            patch("ui._pid_alive", return_value=True),
            patch(
                "ui._process_command",
                return_value="/Applications/Roost-Iced.app/Contents/MacOS/Roost-Iced",
            ),
            patch("ui._wait_pid_gone", return_value=False),
            patch("ui.subprocess.run"),
        ):
            with self.assertRaisesRegex(RuntimeError, "survived SIGKILL"):
                ui._quit_iced_bundle()

    def test_process_name_kill_is_never_used_for_bundle_teardown(self) -> None:
        """Regression guard: teardown must stay pid-based. A `pkill -x
        Roost-Iced` (or `-x Roost`) would be a process-name kill banned by
        plan 027 W5 — this asserts every `subprocess.run` call `kill`s the
        recorded pid directly."""
        with (
            patch("ui._ICED_BUNDLE_PID", 4242),
            patch("ui._pid_alive", side_effect=[True, False]),
            patch(
                "ui._process_command",
                return_value="/Applications/Roost-Iced.app/Contents/MacOS/Roost-Iced",
            ),
            patch("ui._wait_pid_gone", return_value=True),
            patch("ui.subprocess.run") as run,
        ):
            ui._quit_iced_bundle()
        for call in run.call_args_list:
            argv = call.args[0]
            self.assertNotIn("pkill", argv)
            self.assertIn("4242", argv)

    def test_quit_treats_a_reused_pid_as_already_dead_without_signalling_it(
        self,
    ) -> None:
        """macOS can recycle a pid between launch and teardown. A live pid
        whose process no longer names the bundle must be treated as already
        dead — never signalled — or teardown risks killing an unrelated
        process that happened to land on the same number."""
        with (
            patch("ui._ICED_BUNDLE_PID", 4242),
            patch("ui._pid_alive", return_value=True),
            patch("ui._process_command", return_value="/usr/bin/something-else"),
            patch("ui.subprocess.run") as run,
        ):
            ui._quit_iced_bundle()
        run.assert_not_called()
        self.assertIsNone(ui._ICED_BUNDLE_PID)


if __name__ == "__main__":
    unittest.main()
