"""The shell integration's `path` feature (#261): after the user's startup
files, Roost's app-bundle `bin` goes back to the front of `PATH` and its
Claude plugin folder back into `CLAUDE_CODE_PLUGIN_DIRS`.

`__roost_paths` is called directly for the decision table, in both shipped
scripts. The startup-order cases run a real interactive shell on a pty to
its first prompt, through each way the integration loads: a manual
`source` early in an rc file that later resets `PATH`, zsh's ZDOTDIR shim,
and bash's `--posix` + `ENV` injection.

ROOST_TEST_BASH / ROOST_TEST_ZSH pick the shell binary. A missing shell
skips locally and fails under CI.
"""

from __future__ import annotations

import os
import pty
import re
import select
import shutil
import subprocess
import tempfile
import time
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPTS = REPO_ROOT / "crates" / "roost-engine" / "resources" / "shell-integration"

BUNDLE_BIN = "/Applications/Roost.app/Contents/Resources/bin"
ROOSTCTL = f"{BUNDLE_BIN}/roostctl"
PLUGIN = "/c/agent-integration/0123456789abcdef/roost"

SHELLS = {
    "bash": ("ROOST_TEST_BASH", ["--norc", "--noprofile", "-i", "-c"], "roost.bash"),
    "zsh": ("ROOST_TEST_ZSH", ["-f", "-i", "-c"], "roost.zsh"),
}

CALL = (
    '. "$ROOST_TEST_SCRIPT"; __roost_paths; '
    "printf '@@PATH=%s@@\\n' \"$PATH\"; "
    "printf '@@DIRS=%s@@\\n' \"${CLAUDE_CODE_PLUGIN_DIRS-<unset>}\""
)


def shell_path(case: unittest.TestCase, name: str) -> str:
    override = SHELLS[name][0]
    chosen = os.environ.get(override)
    path = shutil.which(chosen or name)
    if path:
        return path
    message = f"{name} not found ({override}={chosen!r})"
    if chosen or os.environ.get("CI"):
        case.fail(message)
    case.skipTest(message)


def read_vars(text: str) -> dict[str, str]:
    """The last `@@NAME=value@@` of each name: a pty echoes the command
    line, whose `%s` comes first, and line editors wrap it in escapes."""
    found = {}
    for name in ("PATH", "DIRS"):
        values = re.findall(rf"@@{name}=(.*?)@@", text)
        if values:
            found[name] = values[-1]
    return found


class PathsCallTest(unittest.TestCase):
    """`__roost_paths` itself, in each shell."""

    def run_paths(self, name: str, env: dict[str, str]) -> dict[str, str]:
        _, flags, script = SHELLS[name]
        with tempfile.TemporaryDirectory(prefix="roost-unit-paths-") as home:
            full = {
                "HOME": home,
                "ROOST_TAB_ID": "1",
                "ROOST_TEST_SCRIPT": str(SCRIPTS / script),
                "LC_ALL": "C",
                **env,
            }
            run = subprocess.run(
                [shell_path(self, name), *flags, CALL],
                env=full,
                stdin=subprocess.DEVNULL,
                capture_output=True,
                start_new_session=True,
                timeout=30,
            )
        self.assertEqual(run.returncode, 0, run.stderr.decode(errors="replace"))
        return read_vars(run.stdout.decode())

    def check(self, env: dict[str, str], path: str, dirs: str | None = None) -> None:
        for name in SHELLS:
            with self.subTest(shell=name, env=env):
                found = self.run_paths(name, env)
                self.assertEqual(found.get("PATH"), path)
                if dirs is not None:
                    self.assertEqual(found.get("DIRS"), dirs)

    def test_a_bundle_bin_moved_back_goes_first_once(self):
        self.check(
            {"ROOST_ROOSTCTL": ROOSTCTL, "PATH": f"/usr/local/bin:{BUNDLE_BIN}:/usr/bin:{BUNDLE_BIN}:/bin"},
            f"{BUNDLE_BIN}:/usr/local/bin:/usr/bin:/bin",
        )

    def test_a_bundle_bin_an_rc_dropped_comes_back(self):
        self.check(
            {"ROOST_ROOSTCTL": ROOSTCTL, "PATH": "/usr/bin:/bin"},
            f"{BUNDLE_BIN}:/usr/bin:/bin",
        )

    def test_a_bundle_bin_already_first_is_left_alone(self):
        path = f"{BUNDLE_BIN}:/usr/bin:{BUNDLE_BIN}"
        self.check({"ROOST_ROOSTCTL": ROOSTCTL, "PATH": path}, path)

    def test_an_app_name_with_pattern_characters_is_matched_literally(self):
        odd = "/Apps/My [Roost]* ?.app/Contents/Resources/bin"
        self.check(
            {"ROOST_ROOSTCTL": f"{odd}/roostctl", "PATH": f"/usr/bin:{odd}:/bin:/Apps/My Xoost.app/Contents/Resources/bin"},
            f"{odd}:/usr/bin:/bin:/Apps/My Xoost.app/Contents/Resources/bin",
        )

    def test_a_roostctl_outside_a_bundle_changes_nothing(self):
        for roostctl in ("/usr/bin/roostctl", "/home/u/roost/target/debug/roostctl", "/x/.app/Contents/Resources/bin/roostctl"):
            self.check({"ROOST_ROOSTCTL": roostctl, "PATH": "/usr/local/bin:/usr/bin"}, "/usr/local/bin:/usr/bin")

    def test_no_roostctl_changes_nothing(self):
        self.check({"PATH": "/usr/local/bin:/usr/bin"}, "/usr/local/bin:/usr/bin")

    def test_no_path_turns_it_off(self):
        self.check(
            {
                "ROOST_ROOSTCTL": ROOSTCTL,
                "PATH": "/usr/bin",
                "ROOST_SHELL_FEATURES": "cwd,no-path",
                "ROOST_CLAUDE_PLUGIN_DIR": PLUGIN,
                "CLAUDE_CODE_PLUGIN_DIRS": "/u/mine",
            },
            "/usr/bin",
            "/u/mine",
        )

    def test_roosts_plugin_folder_is_put_back_after_the_users(self):
        self.check(
            {"PATH": "/usr/bin", "ROOST_CLAUDE_PLUGIN_DIR": PLUGIN, "CLAUDE_CODE_PLUGIN_DIRS": "/u/mine"},
            "/usr/bin",
            f"/u/mine:{PLUGIN}",
        )

    def test_an_unset_plugin_list_gets_roosts_folder_alone(self):
        self.check({"PATH": "/usr/bin", "ROOST_CLAUDE_PLUGIN_DIR": PLUGIN}, "/usr/bin", PLUGIN)

    def test_a_list_holding_roosts_folder_is_left_alone(self):
        dirs = f"{PLUGIN}:/u/mine"
        self.check(
            {"PATH": "/usr/bin", "ROOST_CLAUDE_PLUGIN_DIR": PLUGIN, "CLAUDE_CODE_PLUGIN_DIRS": dirs},
            "/usr/bin",
            dirs,
        )

    def test_nothing_handed_over_is_nothing_put_back(self):
        self.check({"PATH": "/usr/bin", "CLAUDE_CODE_PLUGIN_DIRS": "/u/mine"}, "/usr/bin", "/u/mine")


def first_prompt(argv: list[str], env: dict[str, str], cwd: str) -> dict[str, str]:
    """Start an interactive shell on a pty, wait for its first prompt
    (`READY>`), print the two variables, and exit."""
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(cwd)
        os.execve(argv[0], argv, env)
    out = b""
    sent = False
    deadline = time.monotonic() + 30
    try:
        while time.monotonic() < deadline:
            ready, _, _ = select.select([fd], [], [], 0.2)
            if ready:
                try:
                    chunk = os.read(fd, 65536)
                except OSError:
                    break
                if not chunk:
                    break
                out += chunk
            if not sent and b"READY>" in out:
                os.write(
                    fd,
                    b"printf '@@PATH=%s@@\\n' \"$PATH\"; "
                    b"printf '@@DIRS=%s@@\\n' \"${CLAUDE_CODE_PLUGIN_DIRS-<unset>}\"; exit\n",
                )
                sent = True
    finally:
        try:
            os.kill(pid, 9)
        except ProcessLookupError:
            pass
        os.waitpid(pid, 0)
        os.close(fd)
    return read_vars(out.decode(errors="replace"))


RESET_RC = (
    'export PATH="/rc/first:/usr/bin:/bin"\n'
    'export CLAUDE_CODE_PLUGIN_DIRS="/u/mine"\n'
    "PS1='READY> '\n"
    "PROMPT='READY> '\n"
)


class StartupOrderTest(unittest.TestCase):
    """What a startup file did is undone at the first prompt, however the
    integration was loaded."""

    def setUp(self):
        self.home = tempfile.mkdtemp(prefix="roost-unit-boot-")
        self.addCleanup(shutil.rmtree, self.home, ignore_errors=True)
        self.base_env = {
            "HOME": self.home,
            "TERM": "dumb",
            "LC_ALL": "C",
            "ROOST_TAB_ID": "1",
            "ROOST_ROOSTCTL": ROOSTCTL,
            "ROOST_CLAUDE_PLUGIN_DIR": PLUGIN,
            "CLAUDE_CODE_PLUGIN_DIRS": PLUGIN,
            "PATH": f"{BUNDLE_BIN}:/usr/bin:/bin",
        }

    def assert_restored(self, found: dict[str, str]) -> None:
        self.assertEqual(found.get("PATH"), f"{BUNDLE_BIN}:/rc/first:/usr/bin:/bin", found)
        self.assertEqual(found.get("DIRS"), f"/u/mine:{PLUGIN}", found)

    def test_bash_sourced_early_in_bashrc(self):
        bash = shell_path(self, "bash")
        rc = Path(self.home) / "rc.bash"
        rc.write_text(f'. "{SCRIPTS / "roost.bash"}"\n{RESET_RC}')
        found = first_prompt([bash, "--noprofile", "--rcfile", str(rc), "-i"], self.base_env, self.home)
        self.assert_restored(found)

    def test_bash_injected_through_env(self):
        bash = shell_path(self, "bash")
        (Path(self.home) / ".bashrc").write_text(RESET_RC)
        env = {
            **self.base_env,
            "ENV": str(SCRIPTS / "roost.bash"),
            "ROOST_BASH_INJECT": "1",
        }
        found = first_prompt([bash, "--posix", "-i"], env, self.home)
        self.assert_restored(found)

    def test_zsh_sourced_early_in_zshrc(self):
        zsh = shell_path(self, "zsh")
        (Path(self.home) / ".zshrc").write_text(f'source "{SCRIPTS / "roost.zsh"}"\n{RESET_RC}')
        env = {**self.base_env, "ZDOTDIR": self.home}
        found = first_prompt([zsh, "-i"], env, self.home)
        self.assert_restored(found)

    def test_zsh_through_the_zdotdir_shim(self):
        zsh = shell_path(self, "zsh")
        resources = Path(self.home) / "resources"
        shutil.copytree(SCRIPTS, resources / "shell-integration")
        user = Path(self.home) / "user"
        user.mkdir()
        (user / ".zshrc").write_text(RESET_RC)
        env = {
            **self.base_env,
            "ROOST_RESOURCES_DIR": str(resources),
            "ROOST_ZSH_ZDOTDIR": str(user),
            "ZDOTDIR": str(resources / "shell-integration" / "zsh"),
        }
        found = first_prompt([zsh, "-i"], env, self.home)
        self.assert_restored(found)


if __name__ == "__main__":
    unittest.main()
