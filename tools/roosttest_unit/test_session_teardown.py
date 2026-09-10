"""Fast unit coverage for what `session.SessionEnv.teardown()` deletes.

A lane that gives `make_env` a `root` gives it a directory a live UI is
already using — the socket the client dials and the single-instance lock
it holds both sit under it. Deleting that root at the end of the first
test would take the UI down for every test after it, and the symptom
(the *next* test cannot connect) points nowhere near the cause. So the
line between "the root this env minted" and "the root this env borrowed"
is pinned here: no daemon, no UI, no second process, and a wrong answer
is a directory that is or is not there.
"""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

ROOSTTEST_DIR = Path(__file__).resolve().parents[1] / "roosttest"
sys.path.insert(0, str(ROOSTTEST_DIR))

import session as sessionlib  # noqa: E402


def _fake_session_binary(root: Path) -> Path:
    """An executable standing in for `roost-session`, so `make_env` reads
    the build profile off a path instead of running cargo."""
    binary = root / "target" / "debug" / sessionlib.BIN_NAME
    binary.parent.mkdir(parents=True, exist_ok=True)
    binary.write_text("#!/bin/sh\nexit 0\n")
    binary.chmod(0o755)
    return binary


class TeardownTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory(prefix="roost-teardown-ut-", dir="/tmp")
        self.addCleanup(self.tmp.cleanup)
        self.scratch = Path(self.tmp.name).resolve()
        binary = _fake_session_binary(self.scratch)
        patched = mock.patch.dict(os.environ, {"ROOST_SESSION_BIN": str(binary)})
        patched.start()
        self.addCleanup(patched.stop)

    def _tracked_process(self, env: sessionlib.SessionEnv) -> subprocess.Popen:
        proc = env.track_proc(subprocess.Popen(["sleep", "120"]))

        def reap() -> None:
            if proc.poll() is None:
                proc.kill()
            proc.wait()

        self.addCleanup(reap)
        return proc

    def test_a_borrowed_root_keeps_everything_this_env_did_not_make(self) -> None:
        root = self.scratch / "borrowed"
        # The UI's own runtime directory, made before the env exists and
        # under the same root: the thing a whole-root delete would take.
        theirs = root / "run" / "roost-iced"
        theirs.mkdir(parents=True)
        (theirs / "roost.sock").write_text("")

        env = sessionlib.make_env(root=root)
        self.assertFalse(env.owns_root)
        self.assertIn(root, env.socket.parents)

        # What the daemon would have made, had one run.
        ours = [
            root / "run" / env.namespace,
            root / "data" / env.namespace,
            root / "state" / env.namespace,
        ]
        for made in ours:
            made.mkdir(parents=True, exist_ok=True)
            (made / "witness").write_text("")

        proc = self._tracked_process(env)
        env.teardown()

        self.assertIsNotNone(proc.poll(), "teardown left its tracked process running")
        for made in [*ours, env.launch_cwd, root / "home"]:
            self.assertFalse(made.exists(), f"teardown left {made} behind")
        self.assertTrue(root.is_dir(), "teardown deleted a root it had only borrowed")
        self.assertTrue(
            (theirs / "roost.sock").exists(),
            "teardown deleted the socket directory of whoever lent the root",
        )

    def test_a_private_root_is_removed_whole(self) -> None:
        env = sessionlib.make_env()
        self.assertTrue(env.owns_root)
        root = env.root
        self.assertTrue(root.is_dir())

        proc = self._tracked_process(env)
        env.teardown()

        self.assertIsNotNone(proc.poll(), "teardown left its tracked process running")
        self.assertFalse(root.exists(), "teardown left its own temp root behind")

    def test_a_state_dir_override_reaches_only_the_daemons_environment(self) -> None:
        elsewhere = self.scratch / "ui-state" / "session"
        env = sessionlib.make_env(root=self.scratch / "borrowed", state_dir=elsewhere)
        self.addCleanup(env.teardown)

        self.assertEqual(env.state_dir, elsewhere)
        self.assertEqual(env.env["ROOST_STATE_DIR"], str(elsewhere))
        self.assertEqual(env.state_json, elsewhere / "state.json")

    def test_a_state_dir_is_the_profiles_own_by_default(self) -> None:
        env = sessionlib.make_env()
        self.addCleanup(env.teardown)

        self.assertNotIn("ROOST_STATE_DIR", env.env)
        self.assertIn(env.root, env.state_dir.parents)


if __name__ == "__main__":
    unittest.main()
