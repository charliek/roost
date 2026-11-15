"""Roost's own `OSC 133 ; D` carries the command's exit status (U16).

Sources the shipped bash and zsh shell-integration scripts in real
interactive shells, runs commands with known statuses, and reads the
codes off the `D` marks the prompt hooks print. The shells read their
commands from a pipe, which still runs `PROMPT_COMMAND` and `precmd`
before every prompt, and writes the prompts to stderr.

The status must reach the mark however the user's own prompt hooks are
arranged, and stay the status for what runs after it. In bash a
`PROMPT_COMMAND` entry sees only what the entry before it returned, so
`__roost_marks` has to run first and hand `$?` on; zsh gives every
`precmd` hook the command's status.

ROOST_TEST_BASH / ROOST_TEST_ZSH pick the shell binary. A missing shell
skips locally and fails under CI.
"""

from __future__ import annotations

import os
import re
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPTS = REPO_ROOT / "crates" / "roost-engine" / "resources" / "shell-integration"

SHELLS = {
    "bash": ("ROOST_TEST_BASH", ["--norc", "--noprofile", "-i"]),
    "zsh": ("ROOST_TEST_ZSH", ["-f", "-i"]),
}

MARK = re.compile(rb"\x1b\]133;D(;[^\x1b\x07]*)?(?:\x1b\\|\x07)")

# One line each, so the first prompt with every hook in place follows a
# command that succeeded. The user's own hooks are set up around the
# script, as a `.bashrc` or `.zshrc` would: each returns 0, and each
# prints the status it was handed.
SETUP = {
    "bash": (
        r"""__user_hook() { printf '[hook:%s]' "$?"; return 0; }; """
        r"""PROMPT_COMMAND=__user_hook; PS1='[ps1:$?]'; . "$ROOST_TEST_SCRIPT" """
    ),
    "zsh": (
        r"""autoload -Uz add-zsh-hook; """
        r"""__user_hook() { printf '[hook:%s]' "$?"; return 0; }; """
        r"""__user_later() { printf '[hook:%s]' "$?"; return 0; }; """
        r"""add-zsh-hook precmd __user_hook; PS1='[ps1:%?]'; . "$ROOST_TEST_SCRIPT"; """
        r"""add-zsh-hook precmd __user_later"""
    ),
}
COMMANDS = ["false", "(exit 3)", "true", "sh -c 'kill -TERM $$'"]
# The setup's code, then each command's.
CODES = ["0", "1", "3", "0", "143"]


def _shell(case: unittest.TestCase, name: str) -> str:
    override, _ = SHELLS[name]
    chosen = os.environ.get(override)
    path = shutil.which(chosen or name)
    if path:
        return path
    message = f"{name} not found ({override}={chosen!r}); needed to run the {name} shell integration"
    if chosen or os.environ.get("CI"):
        case.fail(message)
    case.skipTest(message)


def _codes(out: bytes) -> list[str]:
    return [(body or b"").removeprefix(b";").decode(errors="replace") for body in MARK.findall(out)]


def _seen(out: bytes, what: str) -> list[str]:
    return [code.decode() for code in re.findall(rf"\[{what}:(\d+)\]".encode(), out)]


class ExitCodeMarkTest(unittest.TestCase):
    def _run(self, name: str, lines: list[str], features: str = "marks") -> subprocess.CompletedProcess[bytes]:
        _, flags = SHELLS[name]
        with tempfile.TemporaryDirectory(prefix="roost-unit-marks-") as home:
            env = {
                "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                "HOME": home,
                "ROOST_TAB_ID": "1",
                "ROOST_SHELL_FEATURES": features,
                "ROOST_TEST_SCRIPT": str(SCRIPTS / f"roost.{name}"),
                "LC_ALL": "C",
            }
            # A new session keeps the interactive shell off the caller's
            # terminal, which it would otherwise grab.
            run = subprocess.run(
                [_shell(self, name), *flags],
                input="".join(f"{line}\n" for line in [*lines, "exit 0"]).encode(),
                env=env,
                capture_output=True,
                start_new_session=True,
                timeout=30,
            )
        self.assertEqual(run.returncode, 0, run.stderr.decode(errors="replace"))
        return run

    def test_bash_d_carries_the_status_and_the_prompt_still_sees_it(self) -> None:
        run = self._run("bash", [SETUP["bash"], *COMMANDS])
        self.assertEqual(_codes(run.stdout), CODES, run.stdout)
        self.assertEqual(_seen(run.stderr, "ps1")[-len(CODES) :], CODES, run.stderr)

    def test_zsh_d_carries_the_status_whatever_hook_runs_first(self) -> None:
        run = self._run("zsh", [SETUP["zsh"], *COMMANDS])
        self.assertEqual(_codes(run.stdout), CODES, run.stdout)
        # The user's hook runs once before Roost's and once after it.
        self.assertEqual(
            _seen(run.stdout, "hook")[-2 * len(CODES) :],
            [code for code in CODES for _ in range(2)],
            run.stdout,
        )
        self.assertEqual(_seen(run.stderr, "ps1")[-len(CODES) :], CODES, run.stderr)

    def test_bash_hook_hands_the_status_on_with_marks_on_or_off(self) -> None:
        for features in ("marks", "no-marks"):
            with self.subTest(features=features):
                run = self._run(
                    "bash",
                    [
                        '. "$ROOST_TEST_SCRIPT"',
                        r"""(exit 7); __roost_marks; printf '[hook:%s]' "$?" """,
                    ],
                    features,
                )
                self.assertEqual(_seen(run.stdout, "hook"), ["7"], run.stdout)
                if features == "no-marks":
                    self.assertEqual(_codes(run.stdout), [], run.stdout)

    def test_zsh_marks_off_prints_no_mark(self) -> None:
        run = self._run("zsh", [SETUP["zsh"], *COMMANDS], "no-marks")
        self.assertEqual(_codes(run.stdout), [], run.stdout)


if __name__ == "__main__":
    unittest.main()
