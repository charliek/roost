"""Fast unit coverage for the functional harness target contract.

Kept outside ``tools/roosttest`` so pytest's E2E session fixture does not
launch a UI merely to test pure path and capability metadata.
"""

from __future__ import annotations

import fcntl
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
from client import Roost  # noqa: E402


class GeometryContractTests(unittest.TestCase):
    def test_iced_terminal_top_requires_finite_positive_geometry(self) -> None:
        client = object.__new__(Roost)
        self.assertEqual(client.terminal_top({"terminal_top": 34}), 34.0)
        for value in (None, True, 0, -1, float("inf"), float("nan")):
            with self.subTest(value=value):
                with self.assertRaisesRegex(AssertionError, "terminal_top"):
                    client.terminal_top({"terminal_top": value})


class TargetContractTests(unittest.TestCase):
    @patch("ui.subprocess.run")
    def test_mac_test_defaults_cleanup_is_scoped_and_best_effort(self, run) -> None:
        ui._clear_mac_test_defaults()
        run.assert_called_once_with(
            ["defaults", "delete", ui.MAC_TEST_DEFAULTS_SUITE],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )

    def test_target_table_has_three_explicit_profiles(self) -> None:
        self.assertEqual(ui.TARGETS, ("mac", "gtk", "iced"))
        self.assertEqual(ui.TARGET_SPECS["iced"].rust_package, "roost-iced")
        self.assertEqual(ui.TARGET_SPECS["iced"].binary_name, "roost-iced")
        self.assertFalse(ui.TARGET_SPECS["iced"].scans_gtk_criticals)
        self.assertTrue(ui.TARGET_SPECS["mac"].isolates_user_defaults)

    @patch("ui.Path.home", return_value=Path("/Users/tester"))
    @patch("ui.platform.system", return_value="Darwin")
    def test_macos_paths_are_pairwise_distinct(self, _system, _home) -> None:
        paths = {target: ui.socket_path(target) for target in ui.TARGETS}
        self.assertEqual(paths["iced"], Path("/Users/tester/Library/Caches/Roost-iced/roost.sock"))
        self.assertEqual(len(set(paths.values())), 3)

    @patch.dict(os.environ, {"XDG_RUNTIME_DIR": "/run/user/1000"}, clear=False)
    @patch("ui.platform.system", return_value="Linux")
    def test_linux_gtk_and_iced_paths_are_distinct(self, _system) -> None:
        self.assertEqual(ui.socket_path("gtk"), Path("/run/user/1000/roost/roost.sock"))
        self.assertEqual(
            ui.socket_path("iced"), Path("/run/user/1000/roost-iced/roost.sock")
        )

    def test_unknown_target_names_all_candidates(self) -> None:
        with self.assertRaisesRegex(ValueError, r"want mac\|gtk\|iced"):
            ui.socket_path("other")

    @patch.dict(os.environ, {"ROOST_ICED_BIN": "/home/shed/rt/debug/roost-iced"})
    def test_iced_binary_override_is_explicit_and_absolute(self) -> None:
        self.assertEqual(
            ui.rust_binary_path("iced"),
            (Path("/home/shed/rt/debug/roost-iced"), True),
        )

    @patch.dict(os.environ, {"ROOST_GTK_BIN": "out/linux/roost"})
    @patch("ui.REPO_ROOT", Path("/work/roost"))
    def test_relative_gtk_binary_override_is_repository_relative(self) -> None:
        self.assertEqual(
            ui.rust_binary_path("gtk"),
            (Path("/work/roost/out/linux/roost"), True),
        )

    def test_mac_has_no_rust_binary(self) -> None:
        with self.assertRaisesRegex(ValueError, "not a Rust UI"):
            ui.rust_binary_path("mac")

    def test_owned_session_config_is_available_only_inside_harness_state(self) -> None:
        with patch("ui._SESSION_STATE_DIR", None):
            self.assertIsNone(ui.owned_session_config_path())
        with tempfile.TemporaryDirectory() as root:
            state = Path(root)
            with patch("ui._SESSION_STATE_DIR", state):
                path = ui.owned_session_config_path()
            self.assertEqual(path, (state / "launcher.conf").resolve())
            self.assertNotEqual(path, ui.SEED_CONFIG.resolve())

    @patch.dict(os.environ, {"XDG_RUNTIME_DIR": "relative/runtime"}, clear=False)
    @patch("ui.os.getuid", return_value=1234)
    @patch("ui.platform.system", return_value="Linux")
    def test_relative_xdg_runtime_dir_uses_safe_fallback(
        self, _system, _uid
    ) -> None:
        self.assertEqual(
            ui.socket_path("iced"), Path("/tmp/roost-iced-1234/roost.sock")
        )

    def test_the_two_instance_locks_have_distinct_names(self) -> None:
        """Mirrors the Rust invariant: the socket dir and the state dir can be
        the same directory, so one filename would make a process contend with
        itself (flock is per open file description)."""
        with patch("ui.socket_path", return_value=Path("/run/roost/roost.sock")):
            self.assertEqual(ui.socket_lock_path("iced"), Path("/run/roost/roost.lock"))
            self.assertEqual(
                ui.state_lock_path(Path("/run/roost")), Path("/run/roost/state.lock")
            )
            self.assertNotEqual(
                ui.socket_lock_path("iced").name,
                ui.state_lock_path(Path("/run/roost")).name,
            )

    def test_socket_lock_follows_the_socket_not_the_state_dir(self) -> None:
        with patch("ui.socket_path", return_value=Path("/run/user/1/roost/roost.sock")):
            self.assertEqual(
                ui.socket_lock_path("gtk"), Path("/run/user/1/roost/roost.lock")
            )
        self.assertEqual(
            ui.state_lock_path(Path("/tmp/roost-e2e-state-xyz")),
            Path("/tmp/roost-e2e-state-xyz/state.lock"),
        )

    def test_owned_rust_runtime_cleanup_waits_then_removes_socket_and_lock(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            socket = Path(root) / "roost.sock"
            socket_lock = Path(root) / "roost.lock"
            socket.touch()
            socket_lock.touch()
            process = subprocess.Popen(["/usr/bin/true"])
            with (
                patch("ui._ICED_PROC", process),
                patch("ui.socket_path", return_value=socket),
                patch("ui._answering_pid", return_value=None),
            ):
                self.assertEqual(ui.socket_lock_path("iced"), socket_lock)
                ui._cleanup_owned_rust_runtime("iced")
            self.assertFalse(socket.exists())
            self.assertFalse(socket_lock.exists())

    def test_runtime_cleanup_leaves_a_sibling_state_lock_alone(self) -> None:
        """The socket cleanup owns exactly one of the two locks. When the state
        dir happens to be the socket dir, `state.lock` must survive it."""
        with tempfile.TemporaryDirectory() as root:
            socket = Path(root) / "roost.sock"
            socket.touch()
            (Path(root) / "roost.lock").touch()
            state_lock = ui.state_lock_path(Path(root))
            state_lock.touch()
            process = subprocess.Popen(["/usr/bin/true"])
            with (
                patch("ui._ICED_PROC", process),
                patch("ui.socket_path", return_value=socket),
                patch("ui._answering_pid", return_value=None),
            ):
                ui._cleanup_owned_rust_runtime("iced")
            self.assertTrue(state_lock.exists())

    def test_runtime_cleanup_without_owned_process_never_unlinks(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            socket = Path(root) / "roost.sock"
            socket.touch()
            with (
                patch("ui._ICED_PROC", None),
                patch("ui.socket_path", return_value=socket),
            ):
                ui._cleanup_owned_rust_runtime("iced")
            self.assertTrue(socket.exists())

    def test_quit_refuses_a_different_live_socket_owner(self) -> None:
        owned = Mock(pid=123, poll=Mock(return_value=None))
        with (
            patch("ui._ICED_PROC", owned),
            patch("ui._answering_pid", return_value=456),
            patch("ui.subprocess.run") as run,
        ):
            with self.assertRaisesRegex(RuntimeError, "socket owner changed"):
                ui.quit("iced")
        run.assert_not_called()

    def test_quit_refuses_pid_reuse_after_owned_child_exits(self) -> None:
        owned = Mock(pid=123, poll=Mock(return_value=0))
        with (
            patch("ui._ICED_PROC", owned),
            patch("ui._answering_pid", return_value=123),
            patch("ui.subprocess.run") as run,
        ):
            with self.assertRaisesRegex(RuntimeError, "child already exited"):
                ui.quit("iced")
        run.assert_not_called()

    def test_runtime_cleanup_refuses_a_held_replacement_lock(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            socket = Path(root) / "roost.sock"
            socket_lock = Path(root) / "roost.lock"
            socket.touch()
            socket_lock.touch()
            fd = os.open(socket_lock, os.O_RDWR)
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            try:
                process = subprocess.Popen(["/usr/bin/true"])
                with (
                    patch("ui._ICED_PROC", process),
                    patch("ui.socket_path", return_value=socket),
                ):
                    with self.assertRaisesRegex(RuntimeError, "socket lock is held"):
                        ui._cleanup_owned_rust_runtime("iced")
                self.assertTrue(socket.exists())
                self.assertTrue(socket_lock.exists())
            finally:
                os.close(fd)

    def test_runtime_cleanup_refuses_a_replacement_socket_server(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            socket = Path(root) / "roost.sock"
            socket_lock = Path(root) / "roost.lock"
            socket.touch()
            socket_lock.touch()
            process = subprocess.Popen(["/usr/bin/true"])
            with (
                patch("ui._ICED_PROC", process),
                patch("ui.socket_path", return_value=socket),
                patch("ui._answering_pid", return_value=456),
            ):
                with self.assertRaisesRegex(RuntimeError, "replacement iced UI"):
                    ui._cleanup_owned_rust_runtime("iced")
            self.assertTrue(socket.exists())
            self.assertTrue(socket_lock.exists())


class StateLockCleanupTests(unittest.TestCase):
    """`end_session` deletes the session state dir. That dir now holds the
    state lock, so the delete needs the same liveness proof the socket
    cleanup has — an unlinked lock inode is how two UIs come to write one
    `state.json`."""

    def _session(self) -> tuple[Path, Path]:
        root = Path(tempfile.mkdtemp(prefix="roost-unit-state-"))
        self.addCleanup(shutil.rmtree, root, True)
        (root / "state.json").write_text("{}")
        lock = ui.state_lock_path(root)
        lock.touch()
        return root, lock

    def test_end_session_refuses_to_delete_state_while_the_state_lock_is_held(
        self,
    ) -> None:
        state, lock = self._session()
        fd = os.open(lock, os.O_RDWR)
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        try:
            with (
                patch("ui._SESSION_STATE_DIR", state),
                patch("ui.quit"),
                patch("ui._cleanup_owned_rust_runtime"),
            ):
                with self.assertRaisesRegex(RuntimeError, "state lock .* is held"):
                    ui.end_session("iced")
                # Still owned: nothing was deleted, so the next run must not
                # believe the dir is free.
                self.assertIs(ui._SESSION_STATE_DIR, state)
            self.assertTrue(lock.exists())
            self.assertTrue((state / "state.json").exists())
        finally:
            os.close(fd)

    def test_end_session_removes_state_dir_once_the_state_lock_is_free(self) -> None:
        state, _lock = self._session()
        with (
            patch("ui._SESSION_STATE_DIR", state),
            patch("ui.quit"),
            patch("ui._cleanup_owned_rust_runtime"),
        ):
            ui.end_session("iced")
            self.assertIsNone(ui._SESSION_STATE_DIR)
        self.assertFalse(state.exists())

    def test_state_cleanup_refuses_a_replaced_lock_inode(self) -> None:
        """The flock we hold and the inode the *name* resolves to must be the
        same file, or we would be deleting someone else's lock."""
        state, _lock = self._session()
        other_inode = os.stat_result((0,) * 10)
        with patch("ui.os.fstat", return_value=other_inode):
            with self.assertRaisesRegex(RuntimeError, "state lock was replaced"):
                ui._remove_session_state("iced", state)
        self.assertTrue((state / "state.json").exists())

    def test_state_cleanup_without_a_lock_file_just_removes_the_dir(self) -> None:
        state, lock = self._session()
        lock.unlink()
        ui._remove_session_state("iced", state)
        self.assertFalse(state.exists())


class BootRefusalTests(unittest.TestCase):
    """A UI that exits refusing to start (another process holds the state
    lock) must surface as that message, not as a `wait_alive` timeout."""

    REFUSAL = (
        "Error: another Roost (pid 4242) is using this state directory; exiting "
        "rather than writing state.json from two processes."
    )

    def _log(self, text: str) -> Path:
        root = Path(tempfile.mkdtemp(prefix="roost-unit-log-"))
        self.addCleanup(shutil.rmtree, root, True)
        log = root / "roost-iced-ui.log"
        log.write_text(text)
        return log

    def test_nonzero_exit_with_the_refusal_line_is_reported_as_a_refusal(self) -> None:
        log = self._log(f"boot line\n{self.REFUSAL}\n")
        with (
            patch("ui._ICED_PROC", Mock(poll=Mock(return_value=1))),
            patch("ui._ICED_LOG", log),
        ):
            message = ui._boot_refusal("iced")
        self.assertIsNotNone(message)
        assert message is not None
        self.assertIn("pid 4242", message)
        self.assertIn("refusal, not a hang", message)

    def test_a_still_running_ui_is_never_reported_as_a_refusal(self) -> None:
        log = self._log(f"{self.REFUSAL}\n")  # a stale line from a prior launch
        with (
            patch("ui._ICED_PROC", Mock(poll=Mock(return_value=None))),
            patch("ui._ICED_LOG", log),
        ):
            self.assertIsNone(ui._boot_refusal("iced"))

    def test_exit_zero_is_the_activate_path_not_a_refusal(self) -> None:
        log = self._log(f"{self.REFUSAL}\n")
        with (
            patch("ui._ICED_PROC", Mock(poll=Mock(return_value=0))),
            patch("ui._ICED_LOG", log),
        ):
            self.assertIsNone(ui._boot_refusal("iced"))

    def test_mac_refusal_reads_the_app_log_only_while_nothing_runs(self) -> None:
        log = self._log(f"{self.REFUSAL}\n")
        # Offset 0 == "this launch starts at the top of the file", which is
        # what _launch_mac records for a log that did not exist yet.
        with patch("ui._mac_ui_log_path", return_value=log), patch("ui._MAC_LOG_OFFSET", 0):
            with patch("ui._roost_running", return_value=True):
                self.assertIsNone(ui._boot_refusal("mac"))
            with patch("ui._roost_running", return_value=False):
                message = ui._boot_refusal("mac")
        assert message is not None
        self.assertIn("pid 4242", message)

    def test_mac_refusal_is_silent_when_the_harness_never_launched_the_app(self) -> None:
        """A refusal line from a previous day is not this run's refusal.

        Without the `None` sentinel the offset defaulted to 0, so
        `_launch_output("mac")` read the developer's whole accumulated
        `~/Library/Logs/<label>/roost.log` and any historical refusal would
        surface as this launch's, masking the real timeout.
        """
        log = self._log(f"{self.REFUSAL}\n")
        with (
            patch("ui._mac_ui_log_path", return_value=log),
            patch("ui._MAC_LOG_OFFSET", None),
            patch("ui._roost_running", return_value=False),
        ):
            self.assertEqual(ui._launch_output("mac"), "")
            self.assertIsNone(ui._boot_refusal("mac"))

    def test_wait_alive_raises_the_refusal_instead_of_timing_out(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            with (
                patch("ui.socket_path", return_value=Path(root) / "absent.sock"),
                patch("ui._boot_refusal", return_value="iced UI refused to start: x"),
            ):
                # RuntimeError, not TimeoutError: `_launch_mac` retries only
                # TimeoutError, so a refusal must not burn its second attempt.
                with self.assertRaisesRegex(RuntimeError, "refused to start"):
                    ui.wait_alive("iced", timeout=30)


class MacQuitTests(unittest.TestCase):
    def test_quit_gives_a_healthy_app_the_full_scaled_graceful_window(self) -> None:
        """A mid-test relaunch must not be signalled merely for being slow to
        exit; only `_mac_cleanup`'s already-unhealthy app gets the short one."""
        root = Path(tempfile.mkdtemp(prefix="roost-unit-sock-"))
        self.addCleanup(shutil.rmtree, root, True)
        with (
            patch("ui._roost_running", return_value=True),
            patch("ui._wait_gone", return_value=True) as wait_gone,
            patch("ui.subprocess.run"),
            # `_mac_cleanup` unlinks the resolved socket + lock; keep it off
            # the developer's real ones.
            patch("ui.socket_path", return_value=root / "roost.sock"),
        ):
            ui.quit("mac")
            self.assertGreaterEqual(wait_gone.call_args.args[0], 10.0)
            wait_gone.reset_mock()
            ui._mac_cleanup()
            self.assertEqual(wait_gone.call_args.args[0], 3.0)

    def test_quit_escalates_until_the_mac_app_is_confirmed_gone(self) -> None:
        with (
            patch("ui._roost_running", return_value=True),
            patch("ui._wait_gone", side_effect=[False, False, True]),
            patch("ui.subprocess.run") as run,
        ):
            ui.quit("mac")
        self.assertEqual(
            [call.args[0][0] for call in run.call_args_list],
            ["osascript", "pkill", "pkill"],
        )

    def test_quit_raises_rather_than_returning_on_a_mac_app_that_survives_sigkill(
        self,
    ) -> None:
        with (
            patch("ui._roost_running", return_value=True),
            patch("ui._wait_gone", return_value=False),
            patch("ui.subprocess.run"),
        ):
            with self.assertRaisesRegex(RuntimeError, "survived SIGKILL"):
                ui.quit("mac")

    def test_quit_is_a_no_op_when_no_mac_app_runs(self) -> None:
        with (
            patch("ui._roost_running", return_value=False),
            patch("ui.subprocess.run") as run,
        ):
            ui.quit("mac")
        run.assert_not_called()


if __name__ == "__main__":
    unittest.main()
