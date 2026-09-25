"""Roost's own OSC 7 emitters send exactly the bytes its decoders expect
(plan 071 C6, issue #535).

Sources each of the four shipped shell-integration scripts in its real
shell, points `$PWD` at a hostile directory name, calls `__roost_osc7`,
and pins the exact bytes. Every case runs under both a byte locale and a
UTF-8 one, since the encoder walks the path by character.

The Rust and Mac copies deliberately diverge elsewhere, so only the
`__roost_osc7` bodies are compared.

ROOST_TEST_BASH / ROOST_TEST_ZSH pick the shell binary (the Mac copy has
to hold under macOS /bin/bash 3.2). A missing shell skips locally and
fails under CI.
"""

from __future__ import annotations

import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
RUST_DIR = REPO_ROOT / "crates" / "roost-engine" / "resources" / "shell-integration"
MAC_DIR = REPO_ROOT / "mac" / "Sources" / "Roost" / "Resources" / "shell-integration"

HOST = "roost-host"
UTF8_LOCALE = "en_US.UTF-8" if sys.platform == "darwin" else "C.UTF-8"
LOCALES = ("C", UTF8_LOCALE)

SHELLS = {
    ".bash": ("bash", "ROOST_TEST_BASH", ["--norc", "--noprofile", "-i", "-c"]),
    ".zsh": ("zsh", "ROOST_TEST_ZSH", ["-f", "-i", "-c"]),
}

# (label, $PWD, the path as it must appear on the wire)
CASES: list[tuple[str, str, str]] = [
    ("plain", "/home/u/work", "/home/u/work"),
    ("space", "/home/u/a b", "/home/u/a b"),
    ("malformed escape", "/home/u/a%zz", "/home/u/a%25zz"),
    ("valid-looking escape", "/home/u/b%2Fc", "/home/u/b%252Fc"),
    ("trailing percent", "/home/u/100%", "/home/u/100%25"),
    ("ESC", "/home/u/e\x1bx", "/home/u/e%1Bx"),
    ("BEL", "/home/u/b\x07x", "/home/u/b%07x"),
    ("CAN", "/home/u/c\x18x", "/home/u/c%18x"),
    ("SUB", "/home/u/s\x1ax", "/home/u/s%1Ax"),
    ("newline", "/home/u/n\nx", "/home/u/n%0Ax"),
    ("DEL", "/home/u/d\x7fx", "/home/u/d%7Fx"),
    ("non-ASCII", "/home/u/\u00e9t\u00e9/\u65e5\u672c", "/home/u/\u00e9t\u00e9/\u65e5\u672c"),
    ("non-ASCII through the encoder", "/home/u/\u00e9%\x1b\u65e5", "/home/u/\u00e9%25%1B\u65e5"),
    ("C1 code point stays raw", "/home/u/x\u0085y\x01", "/home/u/x\u0085y%01"),
    ("shell metacharacters through the encoder", "/home/u/q'\"\\*?[]$x\x01", "/home/u/q'\"\\*?[]$x%01"),
]

CALL = f'. "$ROOST_TEST_SCRIPT"; HOSTNAME={HOST}; HOST={HOST}; PWD=$ROOST_TEST_PWD; __roost_osc7'

FUNCTION_RE = re.compile(r"^__roost_osc7\(\) \{\n.*?^\}\n", re.MULTILINE | re.DOTALL)


def _osc7_body(script: Path) -> str:
    bodies = FUNCTION_RE.findall(script.read_text(encoding="utf-8"))
    if len(bodies) != 1:
        raise AssertionError(f"{script}: expected one __roost_osc7 definition, found {len(bodies)}")
    return bodies[0]


class Osc7EmitterTest(unittest.TestCase):
    def _shell(self, name: str, override: str) -> str:
        chosen = os.environ.get(override)
        path = shutil.which(chosen or name)
        if path:
            return path
        message = f"{name} not found ({override}={chosen!r}); needed to run the {name} shell integration"
        if chosen or os.environ.get("CI"):
            self.fail(message)
        self.skipTest(message)

    def _assert_emits(self, script: Path) -> None:
        name, override, flags = SHELLS[script.suffix]
        argv = [self._shell(name, override), *flags, CALL]
        with tempfile.TemporaryDirectory(prefix="roost-unit-osc7-") as home:
            env = {
                "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                "HOME": home,
                "ROOST_TAB_ID": "1",
                "ROOST_SHELL_FEATURES": "cwd",
                "ROOST_TEST_SCRIPT": str(script),
            }
            for locale in LOCALES:
                for label, pwd, wire in CASES:
                    with self.subTest(script=script.name, locale=locale, case=label):
                        # A new session keeps the interactive shell off the
                        # caller's terminal, which it would otherwise grab.
                        run = subprocess.run(
                            argv,
                            env={**env, "LC_ALL": locale, "ROOST_TEST_PWD": pwd},
                            stdin=subprocess.DEVNULL,
                            capture_output=True,
                            start_new_session=True,
                            timeout=30,
                        )
                        self.assertEqual(
                            run.stdout,
                            f"\x1b]7;file://{HOST}{wire}\x1b\\".encode(),
                            f"exit {run.returncode}, stderr: {run.stderr.decode(errors='replace')}",
                        )

    def test_rust_bash(self) -> None:
        self._assert_emits(RUST_DIR / "roost.bash")

    def test_mac_bash(self) -> None:
        self._assert_emits(MAC_DIR / "roost.bash")

    def test_rust_zsh(self) -> None:
        self._assert_emits(RUST_DIR / "roost.zsh")

    def test_mac_zsh(self) -> None:
        self._assert_emits(MAC_DIR / "roost.zsh")

    def test_rust_and_mac_bodies_match(self) -> None:
        bash = _osc7_body(RUST_DIR / "roost.bash")
        zsh = _osc7_body(RUST_DIR / "roost.zsh")
        self.assertEqual(bash, _osc7_body(MAC_DIR / "roost.bash"))
        self.assertEqual(zsh, _osc7_body(MAC_DIR / "roost.zsh"))
        self.assertEqual(bash.replace('"${HOSTNAME:-}"', '"${HOST}"'), zsh)


if __name__ == "__main__":
    unittest.main()
