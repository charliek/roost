"""Fast unit coverage for the functional harness target contract.

Kept outside ``tools/roosttest`` so pytest's E2E session fixture does not
launch a UI merely to test pure path and capability metadata.
"""

from __future__ import annotations

import fcntl
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

ROOSTTEST_DIR = Path(__file__).resolve().parents[1] / "roosttest"
sys.path.insert(0, str(ROOSTTEST_DIR))

import ui  # noqa: E402


class TargetContractTests(unittest.TestCase):
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

    @patch.dict(os.environ, {"XDG_RUNTIME_DIR": "relative/runtime"}, clear=False)
    @patch("ui.os.getuid", return_value=1234)
    @patch("ui.platform.system", return_value="Linux")
    def test_relative_xdg_runtime_dir_uses_safe_fallback(
        self, _system, _uid
    ) -> None:
        self.assertEqual(
            ui.socket_path("iced"), Path("/tmp/roost-iced-1234/roost.sock")
        )

    def test_owned_rust_runtime_cleanup_waits_then_removes_socket_and_lock(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            socket = Path(root) / "roost.sock"
            lock = Path(root) / "roost.lock"
            socket.touch()
            lock.touch()
            process = subprocess.Popen(["/usr/bin/true"])
            with (
                patch("ui._ICED_PROC", process),
                patch("ui.socket_path", return_value=socket),
                patch("ui._answering_pid", return_value=None),
            ):
                ui._cleanup_owned_rust_runtime("iced")
            self.assertFalse(socket.exists())
            self.assertFalse(lock.exists())

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
            lock = Path(root) / "roost.lock"
            socket.touch()
            lock.touch()
            fd = os.open(lock, os.O_RDWR)
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            try:
                process = subprocess.Popen(["/usr/bin/true"])
                with (
                    patch("ui._ICED_PROC", process),
                    patch("ui.socket_path", return_value=socket),
                ):
                    with self.assertRaisesRegex(RuntimeError, "lock is held"):
                        ui._cleanup_owned_rust_runtime("iced")
                self.assertTrue(socket.exists())
                self.assertTrue(lock.exists())
            finally:
                os.close(fd)

    def test_runtime_cleanup_refuses_a_replacement_socket_server(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            socket = Path(root) / "roost.sock"
            lock = Path(root) / "roost.lock"
            socket.touch()
            lock.touch()
            process = subprocess.Popen(["/usr/bin/true"])
            with (
                patch("ui._ICED_PROC", process),
                patch("ui.socket_path", return_value=socket),
                patch("ui._answering_pid", return_value=456),
            ):
                with self.assertRaisesRegex(RuntimeError, "replacement iced UI"):
                    ui._cleanup_owned_rust_runtime("iced")
            self.assertTrue(socket.exists())
            self.assertTrue(lock.exists())


if __name__ == "__main__":
    unittest.main()
