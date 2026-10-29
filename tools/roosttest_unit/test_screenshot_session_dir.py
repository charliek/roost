"""tools/screenshot/lib.sh's private session socket dir (plan 083 A1).

Every iced UI the screenshot harness launches gets its own
`ROOST_SESSION_SOCKET_DIR`, and `ut_quit` later stops whatever session
started there and removes the dir. That teardown signals a socket and
runs `rm -rf`, so what is pinned here is that it only ever acts on one of
the harness's own dirs: a recorded path that is anything else — escaping
through `..`, a symlink, the wrong parent or name, not private — is left
alone, a launch whose dir could not be made never happens, and the record
itself is kept only where this user alone can write.

Runs the real bash functions in a subprocess against scratch dirs: a
scratch `HOME` and runtime dir, stand-in sockets, and `/tmp` entries this
test creates and removes. Nothing real is signalled or deleted.
"""

from __future__ import annotations

import json
import os
import shutil
import socket
import subprocess
import tempfile
import threading
import unittest
from pathlib import Path

LIB_SH = Path(__file__).resolve().parents[1] / "screenshot" / "lib.sh"
TMP_ROOT = Path("/tmp").resolve()


def _bash(script: str, env: dict[str, str]) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["bash", "-c", f'set -euo pipefail; . "$LIB_SH"; UT_TARGET=iced; {script}'],
        env={**env, "LIB_SH": str(LIB_SH)},
        capture_output=True,
        text=True,
        timeout=30,
    )


class Listener:
    """A stand-in session on `path`: records every request line it is sent
    and answers each with `reply`."""

    def __init__(self, path: Path, reply: dict):
        self.requests: list[dict] = []
        self._server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._server.bind(str(path))
        self._server.listen(4)
        self._server.settimeout(0.2)
        self._reply = reply
        self._done = threading.Event()
        self._thread = threading.Thread(target=self._serve, daemon=True)
        self._thread.start()

    def _serve(self) -> None:
        while not self._done.is_set():
            try:
                conn, _ = self._server.accept()
            except (TimeoutError, OSError):
                continue
            with conn:
                line = conn.makefile("rb").readline()
                if line:
                    request = json.loads(line)
                    self.requests.append(request)
                    conn.sendall((json.dumps({"id": request["id"], **self._reply}) + "\n").encode())

    def close(self) -> None:
        self._done.set()
        self._thread.join(5)
        self._server.close()


class SessionDirTests(unittest.TestCase):
    def setUp(self) -> None:
        scratch = tempfile.TemporaryDirectory(prefix="roost-ss-ut-", dir="/tmp")
        self.addCleanup(scratch.cleanup)
        self.scratch = Path(scratch.name).resolve()
        self.home = self.scratch / "home"
        self.runtime = self.scratch / "run"
        self.home.mkdir()
        self.runtime.mkdir(mode=0o700)
        self.runtime.chmod(0o700)
        self.env = {
            "PATH": os.environ.get("PATH", ""),
            "HOME": str(self.home),
            "XDG_RUNTIME_DIR": str(self.runtime),
        }
        self.record = self.runtime / "roost-uitest-iced.session-dir"

    def _own_dir(self) -> Path:
        made = _bash("ut_session_dir_new", self.env)
        self.assertEqual(made.returncode, 0, made.stderr)
        path = Path(made.stdout.strip())
        self.addCleanup(shutil.rmtree, path, ignore_errors=True)
        return path

    def _victim(self) -> tuple[Path, Listener]:
        """A directory cleanup must never touch, holding a socket it must
        never signal. Named, owned and moded exactly like one of the
        harness's own, so only its parent tells them apart."""
        victim = self.scratch / "roost-uitest-ss.Victim"
        victim.mkdir(mode=0o700, exist_ok=True)
        listener = Listener(victim / "roost.sock", {"result": {}})
        self.addCleanup(listener.close)
        return victim, listener

    def _cleanup_with(self, recorded: str) -> subprocess.CompletedProcess:
        self.record.write_text(recorded + "\n")
        return _bash("ut_session_dir_cleanup", self.env)

    def test_a_minted_dir_is_private_canonical_and_recorded(self) -> None:
        own = self._own_dir()
        self.assertEqual(own.parent, TMP_ROOT)
        self.assertRegex(own.name, r"^roost-uitest-ss\.[A-Za-z0-9]{6}$")
        self.assertEqual(own.stat().st_mode & 0o777, 0o700)
        self.assertEqual(self.record.read_text().strip(), str(own))

    def test_cleanup_stops_its_own_session_and_removes_its_dir(self) -> None:
        own = self._own_dir()
        listener = Listener(own / "roost.sock", {"result": {}})
        self.addCleanup(listener.close)
        done = _bash("ut_session_dir_cleanup", self.env)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual([r["op"] for r in listener.requests], ["session.stop"])
        self.assertFalse(own.exists())
        self.assertFalse(self.record.exists())

    def test_cleanup_keeps_a_dir_whose_session_did_not_stop(self) -> None:
        own = self._own_dir()
        listener = Listener(own / "roost.sock", {"error": {"code": "busy", "message": "no"}})
        self.addCleanup(listener.close)
        done = _bash("ut_session_dir_cleanup", self.env)
        self.assertEqual(done.returncode, 1, done.stderr)
        self.assertTrue(own.is_dir(), done.stderr)
        self.assertTrue(self.record.is_file(), "the retry needs the record")
        self.assertEqual(self.record.read_text().strip(), str(own))
        self.assertIn("did not stop", done.stderr)

    def test_cleanup_keeps_both_when_the_stop_helper_cannot_run(self) -> None:
        """Only "stopped" and "nothing listening" free the dir: a helper
        that never ran (no python3, killed) says neither."""
        own = self._own_dir()
        listener = Listener(own / "roost.sock", {"result": {}})
        self.addCleanup(listener.close)
        for helper in ("ut_session_stop() { return 127; }", "ut_session_stop() { return 143; }"):
            done = _bash(f"{helper}; ut_session_dir_cleanup", self.env)
            self.assertEqual(done.returncode, 1, f"{helper}: {done.stderr}")
            self.assertTrue(own.is_dir(), helper)
            self.assertTrue(self.record.is_file(), f"{helper}: the retry needs the record")
            self.assertEqual(self.record.read_text().strip(), str(own), helper)

        stubs = (
            "ut_session_stop() { return 127; }; ut_alive() { return 1; }; "
            "ut_pinned_config() { echo /dev/null; }; "
        )
        launch = _bash(f"{stubs}ut_launch", self.env)
        self.assertNotEqual(launch.returncode, 0, launch.stdout + launch.stderr)
        self.assertIn("not launching over its record", launch.stderr)
        self.assertEqual(self.record.read_text().strip(), str(own))

    def test_cleanup_never_touches_a_path_that_is_not_its_own_dir(self) -> None:
        victim, listener = self._victim()
        own = self._own_dir()
        link = TMP_ROOT / f"roost-uitest-ss.{own.name[-6:-1]}L"
        os.symlink(victim, link)
        self.addCleanup(link.unlink)
        shared = TMP_ROOT / f"roost-uitest-ss.{own.name[-6:-1]}S"
        shared.mkdir()
        shared.chmod(0o755)
        self.addCleanup(shutil.rmtree, shared, ignore_errors=True)
        misnamed = TMP_ROOT / f"roost-uitest-ss.{own.name[-6:]}xx"
        misnamed.mkdir(mode=0o700)
        self.addCleanup(shutil.rmtree, misnamed, ignore_errors=True)
        for crafted in (
            f"{own}/../..{victim}",
            f"{own}/../{victim.relative_to(TMP_ROOT)}",
            str(link),
            str(shared),
            str(victim),
            str(misnamed),
            "",
        ):
            done = self._cleanup_with(crafted)
            self.assertEqual(done.returncode, 0, done.stderr)
            self.assertTrue(victim.is_dir() and (victim / "roost.sock").exists(), crafted)
            self.assertTrue(shared.is_dir() and misnamed.is_dir(), crafted)
            self.assertEqual(listener.requests, [], f"{crafted!r} reached the victim's socket")
        self.assertTrue(own.is_dir(), "an unrelated record must not clean the real dir either")

    def test_a_failed_allocation_never_launches(self) -> None:
        fake_repo = self.scratch / "repo"
        fake_ui = fake_repo / "target" / "debug" / "roost-iced"
        fake_ui.parent.mkdir(parents=True)
        launched = self.scratch / "launched"
        fake_ui.write_text(f'#!/bin/sh\necho "$ROOST_SESSION_SOCKET_DIR" > {launched}\n')
        fake_ui.chmod(0o755)
        stubs = (
            f'UT_REPO_ROOT={fake_repo}; ut_alive() {{ return 1; }}; '
            'ut_wait_alive() { return 0; }; rc() { :; }; ut_pinned_config() { echo /dev/null; }; '
        )
        for mktemp in ("mktemp() { return 1; }", "mktemp() { return 0; }"):
            done = _bash(f"{stubs}{mktemp}; ut_launch", self.env)
            self.assertNotEqual(done.returncode, 0, f"{mktemp}: {done.stdout}{done.stderr}")
            self.assertFalse(launched.exists(), f"{mktemp} launched the UI anyway")

    def test_the_record_lives_only_where_this_user_alone_can_write(self) -> None:
        self.runtime.chmod(0o755)
        own = self._own_dir()
        fallback = self.home / ".cache" / "roost-uitest" / "roost-uitest-iced.session-dir"
        self.assertFalse(self.record.exists())
        self.assertEqual(fallback.read_text().strip(), str(own))

        self.env["HOME"] = str(self.scratch / "nowhere")
        (self.scratch / "nowhere").write_text("a file, not a home")
        made = _bash("ut_session_dir_new", self.env)
        self.assertEqual(made.returncode, 0, made.stderr)
        self.addCleanup(shutil.rmtree, made.stdout.strip(), ignore_errors=True)
        self.assertIn("not recorded", made.stderr)


if __name__ == "__main__":
    unittest.main()
