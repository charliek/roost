"""The Mac real-input suite's process and namespace ownership
(`tools/roosttest/real_input_ui.py`, plan 074 §D7).

`ps` is replaced by a table, so these run on any OS: a process is this run's
only when its environment carries this launch's state dir, nothing unproven is
ever signalled, a process `ps` cannot account for is neither signalled nor
forgotten, the namespace's directories go only when this run created and
marked them and holds the namespace's lock while removing them, a silent or
dripping socket cannot stall a poll, and required mode turns the module's
platform skip into a failure.
"""

from __future__ import annotations

import fcntl
import importlib.util
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import unittest
from pathlib import Path
from unittest.mock import patch

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "tools" / "roosttest"))

import real_input_ui  # noqa: E402
import ui  # noqa: E402
from test_real_input_mac_scenarios import scenarios_and_tests  # noqa: E402

APP = Path("/Volumes/x/roost/mac/build/Roost-Iced.app")
EXE = f"{APP}/Contents/MacOS/Roost-Iced"


def fake_ps(table: dict[int, tuple[str, str]], listing: str = ""):
    """A `ps` answering `-o comm=` and `-wwE … -o command=` from `table`
    (pid → executable, environment) and `-axo pid=,comm=` with `listing`."""

    def ps(*args: str) -> str | None:
        if args[0] == "-axo":
            return listing
        pid = int(args[args.index("-p") + 1])
        if pid not in table:
            return None
        executable, environment = table[pid]
        if args[0] == "-wwE":
            return f"{executable} {environment}\n"
        return f"{executable}\n"

    return ps


class OwnershipTests(unittest.TestCase):
    def setUp(self) -> None:
        self.ui = real_input_ui.RealInputUI(APP, {}, real_input_ui.Namespace(Path("/tmp")))
        self.ui.state_dir = Path("/tmp/roost-ri-state-mine")
        self.table = {
            101: (EXE, "ROOST_BUNDLE_PROFILE=linux ROOST_STATE_DIR=/tmp/roost-ri-state-mine"),
            102: (EXE, "ROOST_BUNDLE_PROFILE=linux ROOST_STATE_DIR=/tmp/roost-ri-state-theirs"),
            103: (EXE, "ROOST_BUNDLE_PROFILE=linux"),
            104: ("/usr/bin/other", "ROOST_STATE_DIR=/tmp/roost-ri-state-mine"),
        }
        # 998 is alive but `ps` has no answer for it (a timeout); 999 is gone.
        self.alive = {*self.table, 998}
        for target, name, value in (
            (real_input_ui, "_ps", fake_ps(self.table, f"101 {EXE}\n102 {EXE}\n103 {EXE}\n")),
            (ui, "_pid_alive", lambda pid: pid in self.alive),
        ):
            patcher = patch.object(target, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)

    def test_only_a_process_carrying_this_launchs_state_dir_is_ours(self) -> None:
        self.assertIs(self.ui.owns(101), True)
        self.assertIs(self.ui.owns(102), False, "the same bundle launched by someone else")
        self.assertIs(self.ui.owns(104), False, "our state dir in another executable")
        self.assertIs(self.ui.owns(999), False, "gone")
        self.ui.state_dir = None
        self.assertIs(self.ui.owns(101), False, "before a launch nothing is ours")

    def test_what_ps_cannot_account_for_is_unknown_not_dead(self) -> None:
        self.assertIsNone(self.ui.owns(998), "alive, and ps gave no answer")
        self.assertIsNone(self.ui.owns(103), "the bundle, its environment unreadable")

    def test_strays_are_every_process_not_provably_someone_elses(self) -> None:
        self.assertEqual(self.ui.launched_pids(), {101, 103})

    def test_a_failed_launch_keeps_a_candidate_it_cannot_place_and_its_state(self) -> None:
        state_dir = Path(tempfile.mkdtemp(prefix="ri-state-"))
        self.addCleanup(shutil.rmtree, state_dir, True)
        self.ui.state_dir = state_dir
        self.table[101] = (EXE, f"ROOST_STATE_DIR={state_dir}")
        with (
            patch.object(real_input_ui, "answering_pid", return_value=None),
            patch.object(self.ui, "_adopt", side_effect=AssertionError("adoption failed")),
            patch.object(ui, "_wait_pid_gone", return_value=True),
            patch.object(real_input_ui.subprocess, "run") as run,  # `open`, and any `kill`
        ):
            with self.assertRaises(BaseException) as raised:
                self.ui.launch()
        self.assertIsInstance(raised.exception, RuntimeError, "the cleanup claimed nothing was left")
        self.assertRegex(str(raised.exception), "cannot confirm.*103")
        kills = [call.args[0] for call in run.call_args_list if call.args[0][0] == "kill"]
        self.assertEqual(kills, [["kill", "101"]], "only the proven launch is signalled")
        self.assertEqual(self.ui._strays, {103})
        self.assertEqual(self.ui.state_dir, state_dir)
        self.assertTrue(state_dir.is_dir())

    def test_stop_never_signals_a_process_it_cannot_prove_is_ours(self) -> None:
        self.ui.pid = 102  # say the socket answer was someone else's
        self.ui._strays = {101, 104}
        with (
            patch.object(ui, "_wait_pid_gone", return_value=True),
            patch.object(ui.subprocess, "run") as run,
        ):
            self.ui.stop()
        self.assertEqual([call.args[0] for call in run.call_args_list], [["kill", "101"]])
        self.assertIsNone(self.ui.pid)

    def test_an_unknown_process_is_neither_signalled_nor_forgotten_and_its_state_kept(self) -> None:
        state_dir = Path(tempfile.mkdtemp(prefix="ri-state-"))
        self.addCleanup(shutil.rmtree, state_dir, True)
        self.ui.state_dir = state_dir
        self.table[998] = self.table[103]
        self.ui.pid = 998
        self.ui._strays = {103}
        with patch.object(ui.subprocess, "run") as run:
            with self.assertRaisesRegex(RuntimeError, "cannot confirm.*keeping its state dir"):
                self.ui.quit()
        run.assert_not_called()
        self.assertEqual((self.ui.pid, self.ui._strays), (998, {103}))
        self.assertEqual(self.ui.state_dir, state_dir)
        self.assertTrue(state_dir.is_dir())


class CleanupDecisionTests(unittest.TestCase):
    CREATED = [Path("/x/Caches/Roost-linux"), Path("/x/Logs/Roost-linux")]

    def decide(self, **overrides):
        facts = dict(
            verified_owner=True, foreign_owner=False, answers_now=False, lock_held=False, marked=True
        )
        facts.update(overrides)
        return real_input_ui.removable(self.CREATED, **facts)

    def test_directories_go_only_when_this_run_owned_the_namespace_throughout(self) -> None:
        self.assertEqual(self.decide(), (self.CREATED, None))
        for overrides, reason in (
            ({"verified_owner": False}, "no launch of this run proved"),
            ({"foreign_owner": True}, "another process answered"),
            ({"answers_now": True}, "still answers"),
            ({"lock_held": True}, "lock is held"),
            ({"marked": False}, "no longer holds its marker"),
        ):
            with self.subTest(**overrides):
                kept, why = self.decide(**overrides)
                self.assertEqual(kept, [])
                self.assertIn(reason, why)

    def test_directories_that_existed_before_the_run_are_never_touched(self) -> None:
        self.assertEqual(real_input_ui.removable([], verified_owner=True, foreign_owner=False,
                                                  answers_now=False, lock_held=False), ([], None))


class NamespaceTests(unittest.TestCase):
    def setUp(self) -> None:
        self.root = Path(tempfile.mkdtemp(prefix="ri-namespace-"))
        self.addCleanup(shutil.rmtree, self.root, True)
        caches, logs = self.root / "Caches" / "Roost-linux", self.root / "Logs" / "Roost-linux"
        for name, value in (
            ("CACHES", caches),
            ("LOGS", logs),
            ("SOCKET", caches / "roost.sock"),
            ("SOCKET_LOCK", caches / "roost.lock"),
            ("LOCK", self.root / "roost-real-input.lock"),
            ("answering_pid", lambda: None),
        ):
            patcher = patch.object(real_input_ui, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)
        self.artifacts = self.root / "artifacts"
        self.artifacts.mkdir()
        for library in ("Caches", "Logs"):
            (self.root / library).mkdir()

    def appear(self) -> None:
        """What a launched app does: create both directories and its lock."""
        real_input_ui.CACHES.mkdir(parents=True, exist_ok=True)
        real_input_ui.LOGS.mkdir(parents=True, exist_ok=True)
        (real_input_ui.LOGS / "roost.log").write_text("hello\n")
        real_input_ui.SOCKET_LOCK.touch()

    def test_a_namespace_this_run_owned_is_cleaned_and_its_logs_kept(self) -> None:
        held = real_input_ui.Namespace(self.artifacts)
        self.assertIsNone(held.acquire())
        self.appear()
        held.verified_owner = True
        self.assertIsNone(held.close())
        self.assertFalse(real_input_ui.LOGS.exists())
        self.assertEqual((self.artifacts / "Roost-linux-logs" / "roost.log").read_text(), "hello\n")

    def test_the_lock_and_its_directory_are_never_deleted(self) -> None:
        """The app opens `roost.lock`, then locks it: a launch between the two
        would lock a deleted inode while another locks a new one."""
        held = real_input_ui.Namespace(self.artifacts)
        self.assertIsNone(held.acquire())
        self.appear()
        held.verified_owner = True
        lock = os.stat(real_input_ui.SOCKET_LOCK)
        self.assertIsNone(held.close())
        self.assertTrue(real_input_ui.SOCKET_LOCK.exists(), "roost.lock was deleted")
        after = os.stat(real_input_ui.SOCKET_LOCK)
        self.assertEqual((after.st_dev, after.st_ino), (lock.st_dev, lock.st_ino))
        self.assertEqual(list(real_input_ui.CACHES.iterdir()), [real_input_ui.SOCKET_LOCK])

    def test_the_run_creates_and_marks_the_directories_it_may_remove(self) -> None:
        held = real_input_ui.Namespace(self.artifacts)
        self.assertIsNone(held.acquire())
        self.addCleanup(held.release_lock)
        self.assertEqual(held.created, [real_input_ui.CACHES, real_input_ui.LOGS])
        for directory in held.created:
            self.assertTrue((directory / held.marker).is_file(), directory)
            self.assertTrue(held.marker.startswith(".roost-real-input-run-"))

    def test_a_directory_that_was_already_there_is_neither_marked_nor_removed(self) -> None:
        real_input_ui.CACHES.mkdir()
        held = real_input_ui.Namespace(self.artifacts)
        self.assertIsNone(held.acquire())
        self.assertEqual(held.created, [real_input_ui.LOGS])
        self.assertEqual(list(real_input_ui.CACHES.iterdir()), [])
        self.appear()
        held.verified_owner = True
        self.assertIsNone(held.close())
        self.assertTrue(real_input_ui.CACHES.exists())
        self.assertFalse(real_input_ui.LOGS.exists())

    def test_a_directory_whose_marker_is_gone_keeps_both(self) -> None:
        """Someone removed and re-created it during the run: no longer ours."""
        held = real_input_ui.Namespace(self.artifacts)
        self.assertIsNone(held.acquire())
        self.appear()
        held.verified_owner = True
        (real_input_ui.CACHES / held.marker).unlink()
        self.assertIn("no longer holds its marker", held.close() or "")
        self.assertTrue(real_input_ui.CACHES.exists())
        self.assertTrue(real_input_ui.LOGS.exists())

    def test_the_socket_lock_is_held_by_this_run_throughout_the_removal(self) -> None:
        held = real_input_ui.Namespace(self.artifacts)
        self.assertIsNone(held.acquire())
        self.appear()
        held.verified_owner = True
        removing = []
        real_rmtree = shutil.rmtree

        def rmtree(path, *args, **kwargs):
            # What a UI starting now would do: take the socket lock.
            removing.append((Path(path), real_input_ui.Namespace(self.artifacts).socket_lock_held()))
            real_rmtree(path, *args, **kwargs)

        with patch.object(real_input_ui.shutil, "rmtree", rmtree):
            self.assertIsNone(held.close())
        self.assertEqual(removing, [(real_input_ui.LOGS, True)])
        self.assertFalse(real_input_ui.LOGS.exists())

    def test_a_namespace_someone_else_answered_is_left_with_a_note(self) -> None:
        held = real_input_ui.Namespace(self.artifacts)
        self.assertIsNone(held.acquire())
        self.appear()
        held.verified_owner = True
        held.foreign_owner = True
        self.assertIn("another process", held.close() or "")
        self.assertTrue(real_input_ui.CACHES.exists())
        self.assertIn("another process", (self.artifacts / "namespace-cleanup.txt").read_text())

    def test_a_held_socket_lock_keeps_the_directories(self) -> None:
        held = real_input_ui.Namespace(self.artifacts)
        self.assertIsNone(held.acquire())
        self.appear()
        held.verified_owner = True
        fd = os.open(real_input_ui.SOCKET_LOCK, os.O_RDWR)
        self.addCleanup(os.close, fd)
        fcntl.flock(fd, fcntl.LOCK_EX)
        self.assertIn("lock is held", held.close() or "")
        self.assertTrue(real_input_ui.CACHES.exists())

    def test_a_second_run_cannot_take_a_held_namespace(self) -> None:
        first = real_input_ui.Namespace(self.artifacts)
        self.assertIsNone(first.acquire())
        self.addCleanup(first.release_lock)
        self.assertIn("another real-input run", real_input_ui.Namespace(self.artifacts).acquire())


class SilentSocketTests(unittest.TestCase):
    def test_a_socket_that_accepts_but_never_answers_reads_as_no_answer(self) -> None:
        root = Path(tempfile.mkdtemp(prefix="ri-sock-", dir="/tmp"))
        self.addCleanup(shutil.rmtree, root, True)
        path = root / "s"
        server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        server.bind(str(path))
        server.listen(1)
        self.addCleanup(server.close)
        answers = []
        poll = threading.Thread(
            target=lambda: answers.append(ui._answering_pid_at(path, timeout=0.3)), daemon=True
        )
        poll.start()
        poll.join(5)
        self.assertFalse(poll.is_alive(), "the poll hung on a silent socket")
        self.assertEqual(answers, [None])

    def test_a_socket_that_drips_a_reply_that_never_ends_reads_as_no_answer(self) -> None:
        """Bytes arriving faster than the socket timeout reset it on every
        `recv`; only a deadline on the whole reply bounds the poll."""
        root = Path(tempfile.mkdtemp(prefix="ri-sock-", dir="/tmp"))
        self.addCleanup(shutil.rmtree, root, True)
        path = root / "s"
        server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        server.bind(str(path))
        server.listen(1)
        self.addCleanup(server.close)
        stop = threading.Event()
        self.addCleanup(stop.set)

        def drip() -> None:
            connection, _ = server.accept()
            with connection:
                while not stop.wait(0.05):
                    try:
                        connection.sendall(b"{")
                    except OSError:
                        return

        threading.Thread(target=drip, daemon=True).start()
        answers = []
        poll = threading.Thread(
            target=lambda: answers.append(ui._answering_pid_at(path, timeout=0.3)), daemon=True
        )
        poll.start()
        poll.join(3)
        self.assertFalse(poll.is_alive(), "the poll outlived its deadline on a dripping socket")
        self.assertEqual(answers, [None])


@unittest.skipIf(importlib.util.find_spec("pytest") is None, "this interpreter has no pytest")
class RequiredModeTests(unittest.TestCase):
    """The module's platform and target mismatch skips an ordinary run (the
    Swift lane collects the whole directory) but fails a required one."""

    def run_module(self, required: bool) -> subprocess.CompletedProcess:
        env = {k: v for k, v in os.environ.items() if k not in ("PYTEST_ADDOPTS", "ROOST_TEST_FRESH")}
        env.pop("ROOST_REQUIRE_REAL_INPUT", None)
        if required:
            env["ROOST_REQUIRE_REAL_INPUT"] = "1"
        env["PYTHONDONTWRITEBYTECODE"] = "1"
        return subprocess.run(
            [sys.executable, "-m", "pytest", "-q", "-p", "no:cacheprovider",
             "tools/roosttest/test_real_input_mac.py", "--roost-target", "mac"],
            cwd=REPO, env=env, capture_output=True, text=True, timeout=120, check=False,
        )

    def test_an_ordinary_run_skips(self) -> None:
        result = self.run_module(required=False)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        scenarios, _ = scenarios_and_tests()
        self.assertIn(f"{len(scenarios)} skipped", result.stdout)

    def test_a_required_run_fails(self) -> None:
        result = self.run_module(required=True)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("real input is required", result.stdout)


if __name__ == "__main__":
    unittest.main()
