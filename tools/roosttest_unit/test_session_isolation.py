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
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

ROOSTTEST_DIR = Path(__file__).resolve().parents[1] / "roosttest"
sys.path.insert(0, str(ROOSTTEST_DIR))

import agent_jail  # noqa: E402
import session as sessionlib  # noqa: E402
import ui  # noqa: E402

#: An executable to launch as the UI: macOS has no `/bin/true`.
TRUE = shutil.which("true") or "/usr/bin/true"

_INHERITED = {
    ui.SESSION_SOCKET_DIR_ENV: "/tmp/inherited-socket-dir",
    ui.TEST_PASSWD_HOME_ENV: "/tmp/inherited-passwd-home",
    ui.TEST_OLD_SESSION_DIR_ENV: "/tmp/inherited-old-session-dir",
}


def _named(env, names) -> dict[str, str]:
    """Only `names`, so a failure prints them and not the runner's whole
    environment."""
    return {name: env[name] for name in names if name in env}


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

    def test_the_claim_points_the_runtime_dir_knob_at_this_processs_own(self) -> None:
        knob = ui.TEST_USER_RUNTIME_DIR_ENV
        with mock.patch.dict(os.environ, {"XDG_RUNTIME_DIR": "/tmp/harness-run", knob: "/tmp/inherited"}):
            self._claim()
            self.assertEqual(os.environ[knob], "/tmp/harness-run")

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
            mock.patch.dict(os.environ, {"ROOST_ICED_BIN": TRUE}),
            mock.patch("ui.iced_bundle_app", return_value=None),
            mock.patch("ui.is_alive", return_value=False),
            mock.patch("ui._rust_log_path", return_value=Path(os.devnull)),
            mock.patch("ui.subprocess.Popen", side_effect=popen),
            mock.patch("ui.wait_alive"),
        ):
            ui.launch("iced", force=True)
        self.assertEqual(_isolation(launched), {ui.SESSION_SOCKET_DIR_ENV: str(owned)})

    def test_a_bare_iced_launch_keeps_its_display_drops_ssh_and_names_its_own_runtime_dir(self) -> None:
        """Plan 085 D9: the harness's display and bus reach the UI, the
        runner's SSH login does not, and the runtime-dir knob follows the
        runtime dir the UI really gets — a lane's `extra_env` one."""
        self._claim()
        launched = {}

        def popen(argv, **kwargs):
            launched.update(kwargs["env"])
            return mock.Mock()

        self.addCleanup(setattr, ui, "_ICED_PROC", None)
        self.addCleanup(setattr, ui, "_ICED_LOG", None)
        inherited = {
            "ROOST_ICED_BIN": TRUE,
            "DISPLAY": ":99",
            "DBUS_SESSION_BUS_ADDRESS": "unix:path=/harness/bus",
            "SSH_CONNECTION": "1 2 3 4",
            "SSH_CLIENT": "1 2 3",
            "SSH_TTY": "/dev/pts/1",
            "SSH_AUTH_SOCK": "/runner/agent",
            "HERDR_ENV": "1",
            "XDG_RUNTIME_DIR": "/tmp/runner-run",
            ui.TEST_USER_RUNTIME_DIR_ENV: "/tmp/inherited-knob",
        }
        with (
            mock.patch.dict(os.environ, inherited),
            mock.patch("ui.iced_bundle_app", return_value=None),
            mock.patch("ui.is_alive", return_value=False),
            mock.patch("ui._rust_log_path", return_value=Path(os.devnull)),
            mock.patch("ui.subprocess.Popen", side_effect=popen),
            mock.patch("ui.wait_alive"),
        ):
            ui.launch("iced", force=True, extra_env={"XDG_RUNTIME_DIR": "/tmp/lane-run"})
        self.assertEqual(
            _named(launched, set(inherited) - {"ROOST_ICED_BIN", "XDG_RUNTIME_DIR"}),
            {
                "DISPLAY": ":99",
                "DBUS_SESSION_BUS_ADDRESS": "unix:path=/harness/bus",
                ui.TEST_USER_RUNTIME_DIR_ENV: "/tmp/lane-run",
            },
        )


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

    def test_a_session_env_drops_the_opener_set_and_names_its_own_runtime_dir(self) -> None:
        """Plan 085 D9: no runner's display, bus or SSH login reaches a
        daemon here, and the runtime-dir knob names the daemon's own."""
        runner = {name: f"runner-{name}" for name in ui.OPENER_SET}
        runner |= {"HERDR_ENV": "1", ui.TEST_USER_RUNTIME_DIR_ENV: "/tmp/inherited-knob"}
        with mock.patch.dict(os.environ, runner):
            env = self._make_env()
        self.assertEqual(sorted(set(runner) & set(env.env) - {ui.TEST_USER_RUNTIME_DIR_ENV}), [])
        if sys.platform == "darwin":
            self.assertNotIn(ui.TEST_USER_RUNTIME_DIR_ENV, env.env)
        else:
            self.assertEqual(env.env[ui.TEST_USER_RUNTIME_DIR_ENV], env.env["XDG_RUNTIME_DIR"])

    def test_an_explicit_env_carries_nothing_of_the_runners_but_what_runs_binaries(self) -> None:
        runner = {"PATH": "/runner/bin", "DISPLAY": ":5", "UNRELATED": "x", "LANG": "C.UTF-8"}
        with mock.patch.dict(os.environ, runner):
            env = self._make_env(explicit=True)
        self.assertEqual(
            _named(env.env, {*runner, "ROOST_TEST_MODE", "SHELL"}),
            {"PATH": "/runner/bin", "LANG": "C.UTF-8", "ROOST_TEST_MODE": "1", "SHELL": "/bin/sh"},
        )
        self.assertEqual(_isolation(env.env), {ui.SESSION_SOCKET_DIR_ENV: str(sessionlib.socket_dir(env.root))})
        if sys.platform != "darwin":
            self.assertEqual(env.env[ui.TEST_USER_RUNTIME_DIR_ENV], env.env["XDG_RUNTIME_DIR"])

    def test_a_lent_root_must_agree_with_the_override_its_lender_exported(self) -> None:
        root = self.scratch / "lent"
        with mock.patch.dict(os.environ, {ui.SESSION_SOCKET_DIR_ENV: str(sessionlib.socket_dir(root))}):
            self.assertEqual(self._make_env(root=root).socket.parent, sessionlib.socket_dir(root))
        with mock.patch.dict(os.environ, {ui.SESSION_SOCKET_DIR_ENV: "/tmp/somewhere-else"}):
            with self.assertRaisesRegex(AssertionError, "somewhere-else"):
                sessionlib.make_env(root=root)


class UserRuntimeDirKnobTests(unittest.TestCase):
    """Plan 085 D3/D9: whatever knob the harness inherited, a process it
    starts gets one naming its own runtime dir, or an empty one, which
    derives nothing; never none, which derives the desktop's."""

    KNOB = ui.TEST_USER_RUNTIME_DIR_ENV

    def test_the_knob_follows_the_envs_runtime_dir_and_is_empty_without_one(self) -> None:
        self.assertEqual(
            ui.pin_user_runtime_dir({"XDG_RUNTIME_DIR": "/tmp/own-run", self.KNOB: "/tmp/inherited"}),
            {"XDG_RUNTIME_DIR": "/tmp/own-run", self.KNOB: "/tmp/own-run"},
        )
        self.assertEqual(ui.pin_user_runtime_dir({self.KNOB: "/tmp/inherited"}), {self.KNOB: ""})

    def test_a_jailed_ui_gets_the_jails_runtime_dir_whatever_it_inherited(self) -> None:
        scratch = Path(tempfile.mkdtemp(prefix="roost-isolation-ut-", dir="/tmp")).resolve()
        self.addCleanup(shutil.rmtree, scratch, ignore_errors=True)
        inherited = {"XDG_RUNTIME_DIR": "/tmp/runner-run", self.KNOB: "/tmp/inherited-knob", "WAYLAND_DISPLAY": ""}
        with mock.patch.dict(os.environ, inherited):
            jail = agent_jail.Jail(scratch)
            env = agent_jail.jailed_ui_env(jail)
        self.assertEqual(_named(env, ["XDG_RUNTIME_DIR", self.KNOB]), {
            "XDG_RUNTIME_DIR": str(jail.runtime_dir),
            self.KNOB: str(jail.runtime_dir),
        })

    def test_the_fake_ssh_far_side_gets_the_knob_it_was_handed_or_an_empty_one(self) -> None:
        """`run-remote` runs the remote command under `env -i`: a knob it
        did not forward would leave a far-side daemon none at all."""
        scratch = Path(tempfile.mkdtemp(prefix="roost-isolation-ut-", dir="/tmp")).resolve()
        self.addCleanup(shutil.rmtree, scratch, ignore_errors=True)
        session_env = scratch / "session.env"
        session_env.write_text(f"{self.KNOB}=/tmp/far-run\nexport {self.KNOB}\n")
        fake_ssh = ROOSTTEST_DIR / "fixtures" / "fake-ssh.sh"
        base = {"PATH": "/usr/bin:/bin", "FAKE_SSH_LOG": str(scratch / "log"), "FAKE_SSH_MODE": "run-remote"}

        def far_side(env: dict[str, str]) -> dict[str, str]:
            out = subprocess.run(
                [str(fake_ssh), "host", "env"], env=env, capture_output=True, text=True, check=True, timeout=30
            ).stdout
            return dict(line.split("=", 1) for line in out.splitlines() if "=" in line)

        self.assertEqual(far_side({**base, "FAKE_SSH_SESSION_ENV": str(session_env)}).get(self.KNOB), "/tmp/far-run")
        self.assertEqual(far_side(base).get(self.KNOB), "")


if __name__ == "__main__":
    unittest.main()
