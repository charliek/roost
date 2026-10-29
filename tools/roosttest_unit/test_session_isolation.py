"""Plan 083 D-A6: every process the harness starts names its own session
socket directory, whatever the harness itself inherited.

The product refuses a test-mode process the default session socket, so a
launch that lost the override fails a lane rather than reaching a real
session. What is pinned here is the harness's half — the claim, the UI
launch's environment, and `make_env`'s — with no daemon and no UI.
"""

from __future__ import annotations

import os
import shutil
import stat
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

ROOSTTEST_DIR = Path(__file__).resolve().parents[1] / "roosttest"
sys.path.insert(0, str(ROOSTTEST_DIR))

import session as sessionlib  # noqa: E402
import ui  # noqa: E402

_INHERITED = {
    ui.SESSION_SOCKET_DIR_ENV: "/tmp/inherited-socket-dir",
    ui.TEST_PASSWD_HOME_ENV: "/tmp/inherited-passwd-home",
    ui.TEST_OLD_SESSION_DIR_ENV: "/tmp/inherited-old-session-dir",
}


def _isolation(env) -> dict[str, str]:
    """Only the three names, so a failure prints them and not a whole
    environment."""
    return {name: env[name] for name in ui.SESSION_ISOLATION_ENV if name in env}


class ClaimTests(unittest.TestCase):
    def setUp(self) -> None:
        patched = mock.patch.dict(os.environ, _INHERITED)
        patched.start()
        self.addCleanup(patched.stop)

    def _claim(self) -> tuple[Path, str | None]:
        owned, inherited = ui.claim_session_isolation()
        self.addCleanup(shutil.rmtree, owned, ignore_errors=True)
        return owned, inherited

    def test_the_claim_replaces_every_inherited_knob_with_a_private_dir(self) -> None:
        owned, inherited = self._claim()
        self.assertEqual(inherited, _INHERITED[ui.SESSION_SOCKET_DIR_ENV])
        self.assertEqual(_isolation(os.environ), {ui.SESSION_SOCKET_DIR_ENV: str(owned)})
        self.assertTrue(owned.is_dir())
        self.assertEqual(stat.S_IMODE(owned.stat().st_mode), 0o700)
        self.assertEqual(ui.session_isolation_env(), {ui.SESSION_SOCKET_DIR_ENV: str(owned)})

    def test_a_bare_iced_launch_carries_the_harness_value_not_the_inherited_one(self) -> None:
        owned, _ = self._claim()
        launched = {}

        def popen(argv, **kwargs):
            launched.update(kwargs["env"])
            return mock.Mock()

        self.addCleanup(setattr, ui, "_ICED_PROC", None)
        self.addCleanup(setattr, ui, "_ICED_LOG", None)
        with (
            mock.patch.dict(os.environ, {"ROOST_ICED_BIN": "/bin/true"}),
            mock.patch("ui.iced_bundle_app", return_value=None),
            mock.patch("ui.is_alive", return_value=False),
            mock.patch("ui._rust_log_path", return_value=Path(os.devnull)),
            mock.patch("ui.subprocess.Popen", side_effect=popen),
            mock.patch("ui.wait_alive"),
        ):
            ui.launch("iced", force=True)
        self.assertEqual(_isolation(launched), {ui.SESSION_SOCKET_DIR_ENV: str(owned)})


def _fake_session_binary(root: Path) -> Path:
    binary = root / "target" / "debug" / sessionlib.BIN_NAME
    binary.parent.mkdir(parents=True, exist_ok=True)
    binary.write_text("#!/bin/sh\nexit 0\n")
    binary.chmod(0o755)
    return binary


class MakeEnvTests(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory(prefix="roost-isolation-ut-", dir="/tmp")
        self.addCleanup(tmp.cleanup)
        self.scratch = Path(tmp.name).resolve()
        patched = mock.patch.dict(
            os.environ,
            {**_INHERITED, "ROOST_SESSION_BIN": str(_fake_session_binary(self.scratch))},
        )
        patched.start()
        self.addCleanup(patched.stop)

    def _make_env(self, **kwargs) -> sessionlib.SessionEnv:
        env = sessionlib.make_env(**kwargs)
        self.addCleanup(env.teardown)
        return env

    def test_a_session_env_names_its_own_socket_dir_and_drops_the_test_knobs(self) -> None:
        env = self._make_env()
        own = sessionlib.socket_dir(env.root)
        self.assertEqual(_isolation(env.env), {ui.SESSION_SOCKET_DIR_ENV: str(own)})
        self.assertEqual(env.socket, own / "roost.sock")

    def test_a_binary_that_predates_the_override_keeps_its_default_socket(self) -> None:
        """It binds its platform default whatever it is handed, so the
        override names that directory: this tree's `roostctl` run under the
        same env must reach it too."""
        env = self._make_env(honours_socket_dir=False)
        self.assertNotEqual(env.socket.parent, sessionlib.socket_dir(env.root))
        self.assertEqual(_isolation(env.env), {ui.SESSION_SOCKET_DIR_ENV: str(env.socket.parent)})

    def test_a_lent_root_must_agree_with_the_override_its_lender_exported(self) -> None:
        root = self.scratch / "lent"
        with mock.patch.dict(os.environ, {ui.SESSION_SOCKET_DIR_ENV: str(sessionlib.socket_dir(root))}):
            self.assertEqual(self._make_env(root=root).socket.parent, sessionlib.socket_dir(root))
        with mock.patch.dict(os.environ, {ui.SESSION_SOCKET_DIR_ENV: "/tmp/somewhere-else"}):
            with self.assertRaisesRegex(AssertionError, "somewhere-else"):
                sessionlib.make_env(root=root)


if __name__ == "__main__":
    unittest.main()
