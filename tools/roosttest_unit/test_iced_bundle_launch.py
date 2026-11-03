"""Unit coverage for the `ROOST_ICED_APP` bundle-launch path in
`tools/roosttest/ui.py` (plan 027 C4/W5).

No real launches here — `linux-test`/`e2e-iced-bundle` cover the live path.
This pins: launch-mode selection (bundle vs bare-binary Popen), the three
loud validation errors, and the log-offset/pid bookkeeping the bundle path
uses in place of `_ICED_PROC`.
"""

from __future__ import annotations

import os
import plistlib
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
import uuid
from pathlib import Path
from unittest.mock import Mock, patch

ROOSTTEST_DIR = Path(__file__).resolve().parents[1] / "roosttest"
sys.path.insert(0, str(ROOSTTEST_DIR))

import ui  # noqa: E402

from client import RoostError  # noqa: E402


def _absent_log_path() -> Path:
    """A unique path that is never created — mocks the UI log location
    without `tempfile.mktemp`'s predictable-name hazard (ruff S306)."""
    return Path(tempfile.gettempdir()) / f"roost-absent-{uuid.uuid4().hex}.log"


def _make_bundle(
    root: Path,
    executable_name: str = "Roost-Iced",
    *,
    bundle_id: str = "ai.stridelabs.Roost.iced",
    name: str = "Roost-Iced.app",
    with_executable: bool = True,
) -> Path:
    """A bundle skeleton whose Info.plist declares `bundle_id` and
    `executable_name`. Nothing in it is ever executed."""
    app = root / name
    macos_dir = app / "Contents" / "MacOS"
    macos_dir.mkdir(parents=True)
    with open(app / "Contents" / "Info.plist", "wb") as handle:
        plistlib.dump(
            {"CFBundleIdentifier": bundle_id, "CFBundleExecutable": executable_name}, handle
        )
    if with_executable:
        (macos_dir / executable_name).write_text("#!/bin/sh\n")
    return app


DEV_BUNDLE = ui.IcedBundle(
    app=Path("/Applications/Roost-Iced.app"),
    bundle_id="ai.stridelabs.Roost.iced",
    executable="Roost-Iced",
    profile="iced",
    mac_label="Roost-iced",
    title="Roost-Iced",
)
PRODUCTION_BUNDLE = ui.IcedBundle(
    app=Path("/repo/mac/build/Roost.app"),
    bundle_id="ai.stridelabs.Roost",
    executable="Roost",
    profile="mac",
    mac_label="Roost",
    title="Roost",
)


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
            # The plist names an executable the bundle does not carry.
            app = _make_bundle(Path(root), with_executable=False)
            with patch.dict(os.environ, {"ROOST_ICED_APP": str(app)}):
                with self.assertRaisesRegex(FileNotFoundError, "missing its executable"):
                    ui.iced_bundle_app()

    @patch("ui.platform.system", return_value="Darwin")
    def test_the_executable_checked_is_the_one_the_plist_names(self, _system) -> None:
        with tempfile.TemporaryDirectory() as root:
            app = _make_bundle(Path(root), "Roost", with_executable=False)
            (app / "Contents" / "MacOS" / "Roost-Iced").write_text("#!/bin/sh\n")
            with patch.dict(os.environ, {"ROOST_ICED_APP": str(app)}):
                with self.assertRaisesRegex(FileNotFoundError, r"MacOS/Roost$"):
                    ui.iced_bundle_app()

    @patch("ui.platform.system", return_value="Darwin")
    def test_a_bundle_without_a_plist_raises_loudly(self, _system) -> None:
        with tempfile.TemporaryDirectory() as root:
            app = Path(root) / "Roost-Iced.app"
            (app / "Contents" / "MacOS").mkdir(parents=True)
            (app / "Contents" / "MacOS" / "Roost-Iced").write_text("#!/bin/sh\n")
            with patch.dict(os.environ, {"ROOST_ICED_APP": str(app)}):
                with self.assertRaisesRegex(FileNotFoundError, "no readable"):
                    ui.iced_bundle_app()

    @patch("ui.platform.system", return_value="Darwin")
    def test_valid_bundle_resolves_the_app_path(self, _system) -> None:
        with tempfile.TemporaryDirectory() as root:
            app = _make_bundle(Path(root))
            with patch.dict(os.environ, {"ROOST_ICED_APP": str(app)}):
                self.assertEqual(ui.iced_bundle_app(), app.resolve())

    @patch("ui.platform.system", return_value="Darwin")
    def test_a_bundle_reached_through_a_symlink_owns_its_real_executable(self, _system) -> None:
        """`ps -o comm=` reports the executable's resolved path, so a bundle
        named through a symlinked directory must resolve too, or the harness
        refuses to adopt the process it launched."""
        with tempfile.TemporaryDirectory() as root:
            real = Path(root).resolve() / "real"
            real.mkdir()
            app = _make_bundle(real)
            (Path(root) / "link").symlink_to(real, target_is_directory=True)
            with patch.dict(os.environ, {"ROOST_ICED_APP": str(Path(root) / "link" / app.name)}):
                bundle = ui.iced_bundle()
            assert bundle is not None
            self.assertTrue(bundle.runs_inside(str(app / "Contents" / "MacOS" / "Roost-Iced")))

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
                        (Path(root) / "mac" / "build" / "Roost-Iced.app").resolve(),
                    )


class LaunchModeSelectionTests(unittest.TestCase):
    """`launch("iced", ...)` picks the bundle path only when `ROOST_ICED_APP`
    resolves to something; otherwise the Popen path is untouched."""

    def test_launch_dispatches_to_bundle_helper_when_app_resolves(self) -> None:
        bundle = DEV_BUNDLE
        with (
            patch("ui.is_alive", return_value=False),
            patch("ui._SESSION_STATE_DIR", None),
            patch("ui.iced_bundle", return_value=bundle),
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
            patch("ui.iced_bundle", return_value=None),
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
            patch("ui.iced_bundle", return_value=None),
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
            patch("ui.iced_bundle", return_value=None),
            patch("ui._ICED_BUNDLE_PID", 4242),
            patch("ui._ICED_BUNDLE", DEV_BUNDLE),
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
                ui._assert_bundle_identity_logged(DEV_BUNDLE)

    def test_present_log_line_passes(self) -> None:
        line = (
            'INFO resolved bundle identity bundle_id="ai.stridelabs.Roost.iced" '
            'profile="iced"\n'
        )
        with patch("ui._launch_output", return_value=line):
            ui._assert_bundle_identity_logged(DEV_BUNDLE)  # must not raise

    def test_the_production_bundle_must_resolve_the_mac_profile(self) -> None:
        mac = 'INFO resolved bundle identity bundle_id="ai.stridelabs.Roost" profile="mac"\n'
        with patch("ui._launch_output", return_value=mac):
            ui._assert_bundle_identity_logged(PRODUCTION_BUNDLE)  # must not raise
        iced = 'INFO resolved bundle identity bundle_id="ai.stridelabs.Roost" profile="iced"\n'
        with patch("ui._launch_output", return_value=iced):
            with self.assertRaisesRegex(RuntimeError, 'profile="mac"'):
                ui._assert_bundle_identity_logged(PRODUCTION_BUNDLE)


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

    def setUp(self) -> None:
        bundle = patch("ui._ICED_BUNDLE", None)
        bundle.start()
        self.addCleanup(bundle.stop)

    def test_mismatched_process_name_refuses_to_adopt_the_pid(self) -> None:
        with (
            patch("ui._iced_bundle_ui_log_path", return_value=_absent_log_path()),
            patch("ui.subprocess.run"),
            patch("ui.wait_alive"),
            patch("ui._answering_pid", return_value=4242),
            patch("ui._process_command", return_value="/usr/bin/something-else"),
        ):
            with self.assertRaisesRegex(RuntimeError, "refusing to adopt"):
                ui._launch_iced_bundle(DEV_BUNDLE)
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
            patch("ui._iced_bundle_ui_log_path", return_value=_absent_log_path()),
            patch("ui.subprocess.run") as run,
            patch("ui.wait_alive"),
            patch("ui._answering_pid", return_value=4242),
            patch("ui._process_command", return_value="/Applications/Roost-Iced.app/Contents/MacOS/Roost-Iced"),
            patch("ui._assert_bundle_identity_logged") as assert_identity,
            patch("ui._assert_test_mode_canary") as assert_canary,
        ):
            try:
                ui._launch_iced_bundle(DEV_BUNDLE)
                self.assertEqual(ui._ICED_BUNDLE_PID, 4242)
            finally:
                ui._ICED_BUNDLE_PID = None
        open_argv = run.call_args_list[0].args[0]
        self.assertEqual(open_argv[0], "open")
        self.assertIn("ROOST_TEST_MODE=1", open_argv)
        # A filter that doesn't name roost_iced gets the info floor: the
        # launch path asserts the INFO-level identity line, so a session
        # RUST_LOG=warn (CI's e2e default) must not silence it.
        self.assertIn("RUST_LOG=debug,roost_iced=info", open_argv)
        self.assertFalse(
            any(arg.startswith("ROOST_BUNDLE_PROFILE=") for arg in open_argv),
            f"ROOST_BUNDLE_PROFILE must never be forwarded into the bundle "
            f"launch (it exists to exercise the bundle-id-derived default "
            f"profile): {open_argv!r}",
        )
        assert_identity.assert_called_once()
        assert_canary.assert_called_once_with("iced")

    def _launch_argv_with_env(self, env: dict[str, str]) -> list[str]:
        with (
            tempfile.TemporaryDirectory() as temp_dir,
            patch.dict(os.environ, env, clear=True),
            patch(
                "ui._iced_bundle_ui_log_path",
                return_value=Path(temp_dir) / "iced-ui.log",
            ),
            patch("ui.subprocess.run") as run,
            patch("ui.wait_alive"),
            patch("ui._answering_pid", return_value=4242),
            patch(
                "ui._process_command",
                return_value="/Applications/Roost-Iced.app/Contents/MacOS/Roost-Iced",
            ),
            patch("ui._assert_bundle_identity_logged"),
            patch("ui._assert_test_mode_canary"),
        ):
            try:
                ui._launch_iced_bundle(DEV_BUNDLE)
            finally:
                ui._ICED_BUNDLE_PID = None
        return run.call_args_list[0].args[0]

    def test_an_explicit_roost_iced_rust_log_is_kept_verbatim(self) -> None:
        argv = self._launch_argv_with_env({"RUST_LOG": "warn,roost_iced=debug"})
        self.assertIn("RUST_LOG=warn,roost_iced=debug", argv)
        self.assertFalse(any("roost_iced=info" in a for a in argv))

    def test_unset_rust_log_is_not_forwarded(self) -> None:
        argv = self._launch_argv_with_env({})
        self.assertFalse(any(a.startswith("RUST_LOG=") for a in argv))

    def test_the_harness_session_socket_dir_reaches_the_bundle(self) -> None:
        """`open` drops the caller's environment, so a bundle that missed
        the harness's own session socket dir would be refused its session
        socket under test mode."""
        argv = self._launch_argv_with_env({ui.SESSION_SOCKET_DIR_ENV: "/tmp/roost-ss-x"})
        self.assertIn(f"{ui.SESSION_SOCKET_DIR_ENV}=/tmp/roost-ss-x", argv)

    def test_executable_outside_the_bundle_refuses_to_adopt_the_pid(self) -> None:
        """Same executable *name* as the bundle, but not living inside it —
        e.g. a `Roost-Iced` on $PATH from an unrelated build. Must not be
        adopted even though the process-name check alone would pass."""
        with (
            patch("ui._iced_bundle_ui_log_path", return_value=_absent_log_path()),
            patch("ui.subprocess.run"),
            patch("ui.wait_alive"),
            patch("ui._answering_pid", return_value=4242),
            patch("ui._process_command", return_value="/usr/local/bin/Roost-Iced"),
        ):
            with self.assertRaisesRegex(RuntimeError, "not under the launched bundle"):
                ui._launch_iced_bundle(DEV_BUNDLE)
        self.assertIsNone(ui._ICED_BUNDLE_PID)


class LaunchFailureTeardownTests(unittest.TestCase):
    """`_launch_iced_bundle` must not leave a launched bundle running behind
    any exception raised after the `open` spawn — else a boot-validation
    failure (or a canary failure) leaks a live Roost-Iced process into
    whatever the harness runs next."""

    def setUp(self) -> None:
        bundle = patch("ui._ICED_BUNDLE", None)
        bundle.start()
        self.addCleanup(bundle.stop)

    def test_identity_mismatch_stops_the_bundles_own_processes_never_by_name(
        self,
    ) -> None:
        with (
            patch("ui._ICED_BUNDLE", None),
            patch("ui._iced_bundle_ui_log_path", return_value=_absent_log_path()),
            patch("ui.subprocess.run") as run,
            patch("ui.wait_alive"),
            patch("ui._answering_pid", return_value=4242),
            patch("ui._process_command", return_value="/usr/bin/something-else"),
            patch("ui._stop_bundle_processes") as stop,
        ):
            with self.assertRaisesRegex(RuntimeError, "refusing to adopt"):
                ui._launch_iced_bundle(DEV_BUNDLE)
        self.assertIsNone(ui._ICED_BUNDLE_PID)
        stop.assert_called_once()
        self.assertIs(stop.call_args.args[0], DEV_BUNDLE)
        pkill_calls = [c.args[0] for c in run.call_args_list if c.args[0][0] == "pkill"]
        self.assertEqual(pkill_calls, [])

    def test_canary_failure_after_pid_adoption_pid_kills_the_bundle(self) -> None:
        """Once identity is confirmed and the pid is recorded, a later
        failure (here: the ROOST_TEST_MODE canary) must tear the bundle
        down through the pid-based path, not a name-based `pkill`."""
        with (
            patch.dict(os.environ, {"ROOST_TEST_MODE": "1"}, clear=True),
            patch("ui._iced_bundle_ui_log_path", return_value=_absent_log_path()),
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
                ui._launch_iced_bundle(DEV_BUNDLE)
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
            patch("ui._ICED_BUNDLE", DEV_BUNDLE),
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
            patch("ui._ICED_BUNDLE", DEV_BUNDLE),
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
            patch("ui._ICED_BUNDLE", DEV_BUNDLE),
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

    def test_a_pid_reused_by_an_installed_roost_during_the_grace_period_is_never_killed(
        self,
    ) -> None:
        """The bundle exits during the SIGTERM grace period and an installed
        Roost.app gets its pid: ownership is proven again before SIGKILL, so
        the escalation never reaches it."""
        with (
            patch("ui._ICED_BUNDLE_PID", 4242),
            patch("ui._ICED_BUNDLE", DEV_BUNDLE),
            patch("ui._pid_alive", return_value=True),
            patch(
                "ui._process_command",
                side_effect=[
                    "/Applications/Roost-Iced.app/Contents/MacOS/Roost-Iced",
                    "/Applications/Roost.app/Contents/MacOS/Roost",
                ],
            ),
            # The SIGTERM window ends with the pid alive (the installed app's now);
            # a SIGKILL, were one sent, would "work".
            patch("ui._wait_pid_gone", side_effect=[False, True]),
            patch("ui.subprocess.run") as run,
        ):
            ui._quit_iced_bundle()
            self.assertIsNone(ui._ICED_BUNDLE_PID)
        self.assertEqual([call.args[0] for call in run.call_args_list], [["kill", "4242"]])

    def test_an_identity_ps_cannot_tell_is_neither_signalled_nor_forgotten(self) -> None:
        with (
            patch("ui._ICED_BUNDLE_PID", 4242),
            patch("ui._ICED_BUNDLE", DEV_BUNDLE),
            patch("ui._pid_alive", return_value=True),
            patch("ui._process_command", return_value=None),
            patch("ui.subprocess.run") as run,
        ):
            with self.assertRaisesRegex(ui.OwnershipUnknown, "cannot tell whether pid 4242"):
                ui._quit_iced_bundle()
            self.assertEqual(ui._ICED_BUNDLE_PID, 4242)
        run.assert_not_called()

    def test_a_hung_ps_reads_as_no_answer(self) -> None:
        def hangs(argv, **kwargs):
            raise subprocess.TimeoutExpired(argv, kwargs.get("timeout"))

        with patch("ui.subprocess.run", side_effect=hangs) as run:
            self.assertIsNone(ui._process_command(4242))
        self.assertEqual(run.call_args.kwargs.get("timeout"), ui._PS_TIMEOUT_S)

    def test_process_name_kill_is_never_used_for_bundle_teardown(self) -> None:
        """Regression guard: teardown must stay pid-based. A `pkill -x
        Roost-Iced` (or `-x Roost`) would be a process-name kill banned by
        plan 027 W5 — this asserts every `subprocess.run` call `kill`s the
        recorded pid directly."""
        with (
            patch("ui._ICED_BUNDLE_PID", 4242),
            patch("ui._ICED_BUNDLE", DEV_BUNDLE),
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
            patch("ui._ICED_BUNDLE", DEV_BUNDLE),
            patch("ui._pid_alive", return_value=True),
            patch("ui._process_command", return_value="/usr/bin/something-else"),
            patch("ui.subprocess.run") as run,
        ):
            ui._quit_iced_bundle()
        run.assert_not_called()
        self.assertIsNone(ui._ICED_BUNDLE_PID)


class BundleIdentityTests(unittest.TestCase):
    """`read_bundle` maps the id a bundle declares onto the profile the UI
    resolves for it — MIRRORS `mac_bundle_default_kind`."""

    def _read(self, **kwargs) -> "ui.IcedBundle":
        root = Path(tempfile.mkdtemp(prefix="roost-unit-bundle-id-"))
        self.addCleanup(shutil.rmtree, root, True)
        return ui.read_bundle(_make_bundle(root, **kwargs))

    def test_the_production_id_runs_the_mac_profile(self) -> None:
        bundle = self._read(executable_name="Roost", bundle_id="ai.stridelabs.Roost", name="Roost.app")
        self.assertEqual(
            (bundle.executable, bundle.profile, bundle.mac_label, bundle.title),
            ("Roost", "mac", "Roost", "Roost"),
        )
        self.assertTrue(bundle.is_production)

    def test_the_dev_id_and_any_other_id_run_the_iced_profile(self) -> None:
        for bundle_id in ("ai.stridelabs.Roost.iced", "ai.stridelabs.Roost.other", "com.example.x"):
            with self.subTest(bundle_id=bundle_id):
                bundle = self._read(bundle_id=bundle_id)
                self.assertEqual(
                    (bundle.profile, bundle.mac_label, bundle.title),
                    ("iced", "Roost-iced", "Roost-Iced"),
                )
                self.assertFalse(bundle.is_production)

    def test_a_plist_without_an_identity_is_refused(self) -> None:
        root = Path(tempfile.mkdtemp(prefix="roost-unit-bundle-id-"))
        self.addCleanup(shutil.rmtree, root, True)
        app = _make_bundle(root)
        with open(app / "Contents" / "Info.plist", "wb") as handle:
            plistlib.dump({"CFBundleIdentifier": "ai.stridelabs.Roost"}, handle)
        with self.assertRaisesRegex(ValueError, "CFBundleExecutable"):
            ui.read_bundle(app)
        (app / "Contents" / "Info.plist").write_text("not a plist")
        with self.assertRaisesRegex(ValueError, "not a property list"):
            ui.read_bundle(app)


@patch("ui.platform.system", return_value="Darwin")
@patch("ui.Path.home", return_value=Path("/Users/tester"))
class ProfileDerivedPathTests(unittest.TestCase):
    """Every path the harness derives from a profile follows the bundle's
    identity: the production bundle's are the `Mac` profile's."""

    def _env(self, bundle_id: str, executable: str, name: str) -> dict[str, str]:
        root = Path(tempfile.mkdtemp(prefix="roost-unit-bundle-paths-"))
        self.addCleanup(shutil.rmtree, root, True)
        app = _make_bundle(root, executable, bundle_id=bundle_id, name=name)
        return {"ROOST_ICED_APP": str(app), "CI": "true"}

    def test_the_production_bundle_uses_the_mac_profiles_socket_lock_and_ids(self, _home, _system) -> None:
        with patch.dict(os.environ, self._env("ai.stridelabs.Roost", "Roost", "Roost.app")):
            self.assertEqual(
                ui.socket_path("iced"), Path("/Users/tester/Library/Caches/Roost/roost.sock")
            )
            self.assertEqual(
                ui.socket_lock_path("iced"), Path("/Users/tester/Library/Caches/Roost/roost.lock")
            )
            self.assertEqual(ui.iced_app_id(), "ai.stridelabs.Roost")
            self.assertEqual(ui.iced_app_title(), "Roost")

    def test_a_dev_bundle_keeps_the_iced_profiles_paths(self, _home, _system) -> None:
        with patch.dict(os.environ, self._env("ai.stridelabs.Roost.iced", "Roost-Iced", "Roost-Iced.app")):
            self.assertEqual(
                ui.socket_path("iced"), Path("/Users/tester/Library/Caches/Roost-iced/roost.sock")
            )
            self.assertEqual(ui.iced_app_id(), "ai.stridelabs.Roost.iced")
            self.assertEqual(ui.iced_app_title(), "Roost-Iced")

    def test_the_log_follows_the_launched_bundles_label(self, _home, _system) -> None:
        for bundle, label in ((PRODUCTION_BUNDLE, "Roost"), (DEV_BUNDLE, "Roost-iced"), (None, "Roost-iced")):
            with self.subTest(label=label), patch("ui._ICED_BUNDLE", bundle):
                self.assertEqual(
                    ui._iced_bundle_ui_log_path(),
                    Path(f"/Users/tester/Library/Logs/{label}/roost.log"),
                )


@patch("ui.platform.system", return_value="Darwin")
class ProductionIdentityGuardTests(unittest.TestCase):
    """Plan 089 D2: outside CI the harness refuses the production identity
    unless opted in — before any path derived from it is consulted, because
    on a developer's Mac those are the installed Roost.app's."""

    def setUp(self) -> None:
        root = Path(tempfile.mkdtemp(prefix="roost-unit-bundle-guard-"))
        self.addCleanup(shutil.rmtree, root, True)
        self.production = _make_bundle(root, "Roost", bundle_id="ai.stridelabs.Roost", name="Roost.app")
        self.dev = _make_bundle(root)

    def _env(self, app: Path, **extra: str):
        env = patch.dict(os.environ, {"ROOST_ICED_APP": str(app), **extra})
        env.start()
        self.addCleanup(env.stop)
        for name in ("CI", ui.ALLOW_PRODUCTION_IDENTITY_ENV):
            if name not in extra:
                os.environ.pop(name, None)

    def test_the_production_identity_is_refused_outside_ci(self, _system) -> None:
        for extra in ({}, {ui.ALLOW_PRODUCTION_IDENTITY_ENV: "0"}, {"CI": ""}):
            with self.subTest(extra=extra):
                self._env(self.production, **extra)
                with self.assertRaisesRegex(RuntimeError, ui.ALLOW_PRODUCTION_IDENTITY_ENV):
                    ui.iced_bundle()
                with self.assertRaisesRegex(RuntimeError, "production identity"):
                    ui.socket_path("iced")

    def test_ci_or_the_opt_in_allows_it(self, _system) -> None:
        for extra in ({"CI": "true"}, {ui.ALLOW_PRODUCTION_IDENTITY_ENV: "1"}):
            with self.subTest(extra=extra):
                self._env(self.production, **extra)
                self.assertEqual(ui.iced_bundle().bundle_id, "ai.stridelabs.Roost")

    def test_a_dev_bundle_needs_no_opt_in(self, _system) -> None:
        self._env(self.dev)
        self.assertEqual(ui.iced_bundle().bundle_id, "ai.stridelabs.Roost.iced")

    def test_the_production_identity_never_launches_on_the_real_state_dir(self, _system) -> None:
        with (
            patch("ui._ICED_BUNDLE", None),
            patch("ui.subprocess.run") as run,
        ):
            with self.assertRaisesRegex(RuntimeError, "throwaway state dir"):
                ui._launch_iced_bundle(PRODUCTION_BUNDLE, state_dir=None)
        run.assert_not_called()


class BundleQualifiedCleanupTests(unittest.TestCase):
    """Plan 089 D2: a launch that failed before its pid was confirmed stops
    only processes running the bundle's own executable, by path — never by
    name. The production executable is named `Roost`, as an installed
    Roost.app's process is."""

    def test_an_unrelated_process_with_the_bundles_executable_name_survives(self) -> None:
        name = f"Roost{uuid.uuid4().hex[:8]}"
        root = Path(tempfile.mkdtemp(prefix="roost-unit-same-name-"))
        self.addCleanup(shutil.rmtree, root, True)
        (root / "elsewhere").mkdir()
        link = root / "elsewhere" / name
        # A symlink, so the process takes the link's name without a
        # written-then-executed file, to a binary that keeps running under
        # that name: macOS's /bin/sleep (a framework Python re-execs itself
        # under another name there), and on Linux the interpreter, because
        # uutils' multicall sleep (current ubuntu runners) refuses to run
        # under a name that isn't its own.
        if sys.platform == "darwin":
            link.symlink_to("/bin/sleep")
            argv = ["sleep", "60"]
        else:
            link.symlink_to(Path(sys.executable).resolve())
            argv = ["python3", "-c", "import time; time.sleep(60)"]
        proc = subprocess.Popen(argv, executable=str(link))
        self.addCleanup(proc.wait)
        self.addCleanup(proc.kill)
        deadline = time.monotonic() + 10
        while str(proc.pid) not in subprocess.run(
            ["pgrep", "-x", name], capture_output=True, text=True
        ).stdout.split():
            self.assertLess(time.monotonic(), deadline, f"pgrep never saw {name}")
            time.sleep(0.05)

        bundle = ui.IcedBundle(
            app=root / "Roost.app",
            bundle_id="ai.stridelabs.Roost",
            executable=name,
            profile="mac",
            mac_label="Roost",
            title="Roost",
        )
        asked: list[int] = []
        real = ui._runs_inside

        def spy(pid: int, b: "ui.IcedBundle"):
            asked.append(pid)
            return real(pid, b)

        with patch("ui._runs_inside", side_effect=spy):
            ui._stop_bundle_processes(bundle, graceful=1.0)
        # A signal is delivered asynchronously: give a stopped process the
        # time to die rather than sampling it once.
        with self.assertRaises(
            subprocess.TimeoutExpired, msg="an unrelated process with the bundle's name was stopped"
        ):
            proc.wait(timeout=1.0)
        self.assertIn(proc.pid, asked, "the name matched, so only the path can have spared it")

    def test_only_the_pid_running_inside_the_bundle_is_stopped(self) -> None:
        commands = {
            11: "/repo/mac/build/Roost.app/Contents/MacOS/Roost",
            22: "/Applications/Roost.app/Contents/MacOS/Roost",
        }
        with (
            patch(
                "ui.subprocess.run",
                return_value=subprocess.CompletedProcess(["pgrep"], 0, stdout="11\n22\n"),
            ),
            patch("ui._process_command", side_effect=commands.get),
            patch("ui._terminate_owned_pid") as terminate,
        ):
            ui._stop_bundle_processes(PRODUCTION_BUNDLE, graceful=1.0)
        self.assertEqual([c.args[0] for c in terminate.call_args_list], [11])

    def test_a_production_pid_reused_by_an_installed_roost_app_is_never_signalled(self) -> None:
        """Same process name, different bundle: the name check this replaced
        would have killed the installed app."""
        with (
            patch("ui._ICED_BUNDLE_PID", 4242),
            patch("ui._ICED_BUNDLE", PRODUCTION_BUNDLE),
            patch("ui._pid_alive", return_value=True),
            patch(
                "ui._process_command",
                side_effect=[
                    "/repo/mac/build/Roost.app/Contents/MacOS/Roost",
                    "/Applications/Roost.app/Contents/MacOS/Roost",
                ],
            ),
            patch("ui._wait_pid_gone", side_effect=[False, True]),
            patch("ui.subprocess.run") as run,
        ):
            ui._quit_iced_bundle()
        self.assertEqual([call.args[0] for call in run.call_args_list], [["kill", "4242"]])


if __name__ == "__main__":
    unittest.main()
