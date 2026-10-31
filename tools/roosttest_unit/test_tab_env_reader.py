"""`tools/roosttest/tab_env.py`: a process's environment read back exactly,
and the fence that keeps the reader to processes a test started (plan 085
D9). Every child here is started with an explicit environment, so nothing
is read from, or compared with, the runner's own.
"""

from __future__ import annotations

import os
import shutil
import struct
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

ROOSTTEST_DIR = Path(__file__).resolve().parents[1] / "roosttest"
sys.path.insert(0, str(ROOSTTEST_DIR))

import tab_env  # noqa: E402

SLEEPER = tab_env.sleeper()

#: Values that break a space-joined `ps` reading, and one that is empty.
AWKWARD = {
    "PATH": "/usr/bin:/bin",
    "SSH_CONNECTION": "10.0.0.2 51000 10.0.0.1 22",
    "SPACED": "a  b c",
    "EQUALS": "k=v=w",
    "EMPTY": "",
}


def _spawn(argv: list[str], env: dict[str, str]) -> subprocess.Popen:
    return subprocess.Popen(argv, env=env)


def _placed_environ(pid: int) -> dict[str, str]:
    """`_environ(pid)` once the kernel has placed it. `Popen` can return
    while the child is still inside `execve` — a `vfork`ed parent resumes
    before the new environment is laid out — and the block reads empty
    until then."""
    deadline = time.monotonic() + 5
    while not (env := tab_env._environ(pid)) and time.monotonic() < deadline:
        time.sleep(0.01)
    return env


class EnvironTests(unittest.TestCase):
    def test_a_live_childs_environment_reads_back_exactly(self) -> None:
        """On macOS this is the `KERN_PROCARGS2` reader against a live
        child; on Linux `/proc/<pid>/environ`."""
        child = _spawn(tab_env.sleep_argv(30), AWKWARD)
        self.addCleanup(child.wait)
        self.addCleanup(child.kill)
        self.assertEqual(_placed_environ(child.pid), AWKWARD)

    def test_an_environment_reads_back_exactly_whatever_word_it_ends_on(self) -> None:
        """Read as soon as it is placed, which on macOS can be before the
        process has zeroed Apple's strings: one of these lengths ends the
        environment on a word, with no NUL before Apple's first string."""
        for length in range(8):
            env = {"PATH": "/usr/bin:/bin", "PAD": "x" * length}
            child = _spawn(tab_env.sleep_argv(30), env)
            try:
                self.assertEqual(_placed_environ(child.pid), env, f"PAD of {length}")
            finally:
                child.kill()
                child.wait()

    def test_a_gone_process_raises(self) -> None:
        child = _spawn(tab_env.sleep_argv(0), {"PATH": "/usr/bin:/bin"})
        child.wait()
        with self.assertRaises(OSError):
            tab_env._environ(child.pid)

    @unittest.skipUnless(sys.platform == "darwin", "only macOS withholds an environment")
    def test_a_withheld_environment_is_an_error_not_an_empty_one(self) -> None:
        child = _spawn(["/bin/sleep", "30"], AWKWARD)
        self.addCleanup(child.wait)
        self.addCleanup(child.kill)
        deadline = time.monotonic() + 5
        while tab_env.exe_of(child.pid) != Path("/bin/sleep") and time.monotonic() < deadline:
            time.sleep(0.01)
        with self.assertRaises(tab_env.HiddenEnvironment):
            tab_env._environ(child.pid)


def _procargs2(argv: list[bytes], env: list[bytes], apple: list[bytes]) -> bytes:
    """A `KERN_PROCARGS2` buffer in the kernel's layout: argc, the exec
    path, NUL padding to a word, argv, env, an empty string, then Apple's
    strings."""
    exec_path = argv[0] + b"\0"
    exec_path += b"\0" * (-len(exec_path) % 8)
    strings = b"".join(s + b"\0" for s in argv + env) + b"\0"
    return struct.pack("=i", len(argv)) + exec_path + strings + b"".join(s + b"\0" for s in apple)


class Procargs2ParserTests(unittest.TestCase):
    def test_the_environment_ends_at_the_first_empty_string(self) -> None:
        buf = _procargs2(
            [b"/bin/sleep", b"30"],
            [f"{name}={value}".encode() for name, value in AWKWARD.items()],
            [b"ptr_munge=abc", b"executable_path=/bin/sleep"],
        )
        self.assertEqual(tab_env.parse_procargs2(buf), AWKWARD)

    def test_an_argument_with_an_equals_sign_is_not_an_environment_string(self) -> None:
        buf = _procargs2([b"/usr/bin/env", b"A=1", b"-i"], [b"B=2"], [])
        self.assertEqual(tab_env.parse_procargs2(buf), {"B": "2"})

    def test_an_empty_environment_reads_empty(self) -> None:
        buf = _procargs2([b"/bin/sleep"], [], [b"ptr_munge="])
        self.assertEqual(tab_env.parse_procargs2(buf), {})

    def test_the_environment_ends_at_apples_first_string_when_nothing_pads_it(self) -> None:
        """A read right after `execve`, before the process has zeroed
        Apple's first strings, of an environment that ends on a word: no
        NUL between the two."""
        buf = _procargs2([b"/bin/sleep", b"30"], [b"A=1", b"B=2"], [])[:-1]
        buf += b"".join(s + b"\0" for s in [b"pfz=0xffff64000", b"stack_guard=0x52", b"ptr_munge="])
        self.assertEqual(tab_env.parse_procargs2(buf), {"A": "1", "B": "2"})

    def test_a_buffer_that_ends_with_the_arguments_is_a_withheld_environment(self) -> None:
        """What macOS returns for a platform binary: the arguments and
        nothing after them, not even the empty string an empty
        environment ends with."""
        full = _procargs2([b"/bin/sleep", b"30"], [], [])
        with self.assertRaises(tab_env.HiddenEnvironment):
            tab_env.parse_procargs2(full[:-1])


#: A stand-in server: runs the argv after its first argument once per id
#: in that comma-separated first argument, each with ROOST_TAB_ID set to
#: that id, plus once without, prints their pids and waits to be killed.
FAKE_SERVER = """
import os, subprocess, sys, time
kids = [subprocess.Popen(sys.argv[2:], env={"ROOST_TAB_ID": tab})
        for tab in sys.argv[1].split(",")]
kids.append(subprocess.Popen(sys.argv[2:], env={}))
print(" ".join(str(kid.pid) for kid in kids), flush=True)
time.sleep(60)
"""


#: Starts its argv in a new session, prints the pid and exits, so the
#: child is reparented away from this process: a process this test did
#: not start, as far as ancestry can tell.
ORPHANER = """
import subprocess, sys
child = subprocess.Popen(sys.argv[1:], start_new_session=True, stdin=subprocess.DEVNULL,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
print(child.pid, flush=True)
"""


def _orphan(case: unittest.TestCase, argv: list[str], env: dict[str, str]) -> int:
    out = subprocess.run(
        [sys.executable, "-c", ORPHANER, *argv], env=env, capture_output=True, text=True, check=True
    )
    pid = int(out.stdout)
    case.addCleanup(lambda: _kill_group(pid))
    return pid


def _kill_group(pgid: int) -> None:
    try:
        os.killpg(pgid, 9)
    except ProcessLookupError:
        pass


def _scratch_copy(case: unittest.TestCase, binary: Path) -> Path:
    scratch = Path(tempfile.mkdtemp(prefix="roost-tab-env-ut-"))
    case.addCleanup(shutil.rmtree, scratch, ignore_errors=True)
    return tab_env.private_copy(binary, scratch / Path(binary).name)


class OwnedProcessesTests(unittest.TestCase):
    """Every binary here is a scratch copy, so even a fence that failed
    could read no process but this test's."""

    def _pids(self, **binaries: list[Path]) -> set[int]:
        return {pid for pid, _ in tab_env.owned_processes(**binaries)}

    def _child(self, binary: Path, env: dict[str, str]) -> subprocess.Popen:
        child = _spawn(tab_env.sleep_argv(30, binary), env)
        self.addCleanup(child.wait)
        self.addCleanup(child.kill)
        _placed_environ(child.pid)
        return child

    def _placed_orphan(self, binary: Path, env: dict[str, str]) -> int:
        pid = _orphan(self, tab_env.sleep_argv(30, binary), env)
        _placed_environ(pid)
        return pid

    def test_a_private_binary_is_the_tests_wherever_it_runs(self) -> None:
        sleep = _scratch_copy(self, SLEEPER)
        orphan = self._placed_orphan(sleep, {"MARK": "orphan"})
        self.assertIn(orphan, self._pids(private=[sleep]))

    def test_a_shared_binary_is_the_tests_only_under_this_process(self) -> None:
        sleep = _scratch_copy(self, SLEEPER)
        child = self._child(sleep, {"MARK": "child"})
        orphan = self._placed_orphan(sleep, {"MARK": "orphan"})
        found = self._pids(shared=[sleep])
        self.assertIn(child.pid, found)
        self.assertNotIn(orphan, found, "a process outside this one's tree was read")

    def test_a_process_outside_this_ones_tree_is_never_read(self) -> None:
        """Not read at all, rather than read and then dropped."""
        sleep = _scratch_copy(self, SLEEPER)
        child = self._child(sleep, {"MARK": "child"})
        orphan = self._placed_orphan(sleep, {"MARK": "orphan"})
        real = tab_env._environ_at
        read: list[int] = []

        def environ_at(pid: int, fd: int | None) -> dict[str, str]:
            read.append(pid)
            return real(pid, fd)

        with mock.patch.object(tab_env, "_environ_at", environ_at):
            tab_env.owned_processes(shared=[sleep])
        self.assertIn(child.pid, read)
        self.assertNotIn(orphan, read, "the outsider's environment was read")

    @unittest.skipIf(sys.platform == "darwin", "macOS has no handle to pin a process by")
    def test_a_read_follows_the_process_it_pinned_not_its_pid(self) -> None:
        """What makes a reused pid unreadable: the held `/proc/<pid>`
        answers for the process it was opened on, and for no other."""
        sleep = _scratch_copy(self, SLEEPER)
        first = self._child(sleep, {"MARK": "first"})
        second = self._child(sleep, {"MARK": "second"})
        with tab_env._pinned(first.pid) as fd:
            self.assertEqual(tab_env._environ_at(second.pid, fd), {"MARK": "first"})
            first.kill()
            first.wait()
            with self.assertRaises(OSError):
                tab_env._environ_at(first.pid, fd)

    @unittest.skipUnless(sys.platform == "darwin", "Linux pins the process instead")
    def test_a_pid_that_changes_identity_across_the_read_yields_nothing(self) -> None:
        sleep = _scratch_copy(self, SLEEPER)
        child = self._child(sleep, {"MARK": "child"})
        real_environ, real_identity = tab_env._environ, tab_env._identity
        read: set[int] = set()

        def environ(pid: int) -> dict[str, str]:
            env = real_environ(pid)
            read.add(pid)
            return env

        def identity(pid: int):
            found = real_identity(pid)
            return (found[0], "reused") if found and pid in read else found

        with mock.patch.object(tab_env, "_environ", environ), mock.patch.object(tab_env, "_identity", identity):
            found = self._pids(private=[sleep])
        self.assertIn(child.pid, read, "the child's environment was never read")
        self.assertNotIn(child.pid, found)


class TabProcessTests(unittest.TestCase):
    def _server(self, *tab_ids: str) -> tuple[subprocess.Popen, list[int]]:
        server = subprocess.Popen(
            [sys.executable, "-c", FAKE_SERVER, ",".join(tab_ids), *tab_env.sleep_argv(30)],
            stdout=subprocess.PIPE,
            text=True,
            start_new_session=True,
        )

        def stop() -> None:
            os.killpg(server.pid, 9)
            server.wait()
            server.stdout.close()

        self.addCleanup(stop)
        return server, [int(pid) for pid in server.stdout.readline().split()]

    def test_the_one_child_carrying_the_tab_id_is_found(self) -> None:
        server, kids = self._server("7", "8")
        python = tab_env.exe_of(os.getpid())
        pid, env = tab_env.tab_process(server.pid, 8, binaries=[python], timeout=10)
        self.assertEqual((pid, env), (kids[1], {"ROOST_TAB_ID": "8"}))

    def test_a_server_running_another_binary_is_refused_before_anything_is_read(self) -> None:
        server, _ = self._server("7")
        with tempfile.TemporaryDirectory() as scratch:
            elsewhere = Path(scratch) / "roost-session"
            elsewhere.write_text("")
            with self.assertRaisesRegex(AssertionError, "refusing"):
                tab_env.tab_process(server.pid, 7, binaries=[elsewhere], timeout=1)

    def test_a_server_this_process_did_not_start_is_refused_before_anything_is_read(self) -> None:
        """Its children are this test's own, so a fence that failed would
        find tab 7 among them rather than read anyone else's."""
        server = _orphan(self, [sys.executable, "-c", FAKE_SERVER, "7", *tab_env.sleep_argv(30)], {})
        python = tab_env.exe_of(server)
        with self.assertRaisesRegex(AssertionError, "not a child of this test"):
            tab_env.tab_process(server, 7, binaries=[python], timeout=10)

    def test_two_children_claiming_one_tab_is_an_error(self) -> None:
        server, _ = self._server("7", "7")
        python = tab_env.exe_of(os.getpid())
        with self.assertRaisesRegex(AssertionError, "all claim tab 7"):
            tab_env.tab_process(server.pid, 7, binaries=[python], timeout=10)

    def test_no_child_carrying_the_tab_id_times_out(self) -> None:
        server, _ = self._server("7")
        python = tab_env.exe_of(os.getpid())
        with self.assertRaisesRegex(AssertionError, "no child"):
            tab_env.tab_process(server.pid, 9, binaries=[python], timeout=0.5)


class ChildEnvironTests(unittest.TestCase):
    def test_a_child_running_one_of_the_binaries_is_read(self) -> None:
        child = _spawn(tab_env.sleep_argv(30), AWKWARD)
        self.addCleanup(child.wait)
        self.addCleanup(child.kill)
        _placed_environ(child.pid)
        self.assertEqual(tab_env.child_environ(child.pid, binaries=[SLEEPER]), AWKWARD)

    def test_a_child_running_another_binary_is_refused(self) -> None:
        child = _spawn(tab_env.sleep_argv(30), AWKWARD)
        self.addCleanup(child.wait)
        self.addCleanup(child.kill)
        with tempfile.TemporaryDirectory() as scratch:
            elsewhere = Path(scratch) / "roost-iced"
            elsewhere.write_text("")
            with self.assertRaisesRegex(AssertionError, "refusing"):
                tab_env.child_environ(child.pid, binaries=[elsewhere])

    def test_a_process_this_one_did_not_start_is_refused(self) -> None:
        orphan = _orphan(self, tab_env.sleep_argv(30), {"PATH": "/usr/bin:/bin"})
        with self.assertRaisesRegex(AssertionError, "not a child of this test"):
            tab_env.child_environ(orphan, binaries=[tab_env.exe_of(orphan)])


class LocalSetTests(unittest.TestCase):
    def test_the_local_set_is_taken_and_a_relative_wayland_display_made_absolute(self) -> None:
        env = {
            "DISPLAY": ":1",
            "WAYLAND_DISPLAY": "wayland-2",
            "XDG_RUNTIME_DIR": "/run/user/7",
            "SSH_AUTH_SOCK": "",
            "SSH_CONNECTION": "1 2 3 4",
        }
        self.assertEqual(
            tab_env.local_set_from(env),
            {"DISPLAY": ":1", "WAYLAND_DISPLAY": "/run/user/7/wayland-2", "SSH_AUTH_SOCK": ""},
        )
        self.assertEqual(
            tab_env.local_set_from({"WAYLAND_DISPLAY": "wayland-2"}),
            {"WAYLAND_DISPLAY": "wayland-2"},
            "no runtime dir to resolve it against",
        )

    def test_a_value_over_the_byte_limit_is_skipped(self) -> None:
        at_limit = "€" * (tab_env.MAX_LOCAL_VALUE // 3) + "a"
        self.assertEqual(len(at_limit.encode()), tab_env.MAX_LOCAL_VALUE)
        self.assertEqual(
            tab_env.local_set_from({"DISPLAY": "a" * 4097, "SSH_AUTH_SOCK": at_limit}),
            {"SSH_AUTH_SOCK": at_limit},
            "the limit counts UTF-8 bytes, not characters",
        )
        self.assertEqual(
            tab_env.local_set_from({"DISPLAY": at_limit + "a"}),
            {},
        )

    def test_a_wayland_display_over_the_limit_once_made_absolute_is_skipped(self) -> None:
        display = "w" * (tab_env.MAX_LOCAL_VALUE - 4)
        self.assertEqual(
            tab_env.local_set_from({"WAYLAND_DISPLAY": display, "XDG_RUNTIME_DIR": "/run"}),
            {},
        )
        self.assertEqual(
            tab_env.local_set_from({"WAYLAND_DISPLAY": display, "XDG_RUNTIME_DIR": "/r"}),
            {"WAYLAND_DISPLAY": "/r/" + display},
        )

    def test_a_value_with_a_nul_is_skipped(self) -> None:
        self.assertEqual(
            tab_env.local_set_from({"DISPLAY": ":1\0", "SSH_AUTH_SOCK": "/a"}),
            {"SSH_AUTH_SOCK": "/a"},
        )

    def test_a_value_that_is_not_utf8_is_skipped(self) -> None:
        env = {"DISPLAY": os.fsdecode(b":1\xff"), "SSH_AUTH_SOCK": "/a"}
        self.assertEqual(tab_env.local_set_from(env), {"SSH_AUTH_SOCK": "/a"})

    def test_a_runtime_dir_that_is_not_utf8_leaves_a_relative_display_as_is(self) -> None:
        env = {
            "WAYLAND_DISPLAY": "wayland-2",
            "XDG_RUNTIME_DIR": os.fsdecode(b"/run/\xff"),
        }
        self.assertEqual(tab_env.local_set_from(env), {"WAYLAND_DISPLAY": "wayland-2"})


if __name__ == "__main__":
    unittest.main()
