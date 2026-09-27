"""Roost's own OSC 7/0 emitters send exactly the bytes its decoders expect
(plan 071 C6, issue #535; plan 072 D11, issues #554/#193).

Sources each of the four shipped shell-integration scripts in its real
shell, points `$PWD` at a hostile directory name, calls `__roost_osc7` /
`__roost_title`, and pins the exact bytes. Every OSC 7 case runs under both
a byte locale and a UTF-8 one, since that encoder walks the path by
character; `__roost_title` needs no such round trip, so a control byte is
just replaced with `?`.

The Rust and Mac copies deliberately diverge elsewhere (the bash inject
block, the default prompt), so only the shared functions are compared,
guard wrapper included: bash's `__roost_osc7`, `__roost_title`,
`__roost_marks`; zsh's `__roost_osc7`, `__roost_title`, `__roost_mark_c`,
`__roost_mark_d`.

ROOST_TEST_BASH / ROOST_TEST_ZSH pick the shell binary (the Mac copy has
to hold under macOS /bin/bash 3.2). A missing shell skips locally and
fails under CI.
"""

from __future__ import annotations

import os
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

BASH_SHARED_FUNCTIONS = ("__roost_osc7", "__roost_title", "__roost_marks")
ZSH_SHARED_FUNCTIONS = ("__roost_osc7", "__roost_title", "__roost_mark_c", "__roost_mark_d")

# (label, $PWD, the path as it must appear on the OSC 7 wire)
OSC7_CASES: list[tuple[str, str, str]] = [
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

# (label, $PWD, the path as it must appear on the OSC 0 title wire)
TITLE_CASES: list[tuple[str, str, str]] = [
    ("plain", "/home/u/work", "/home/u/work"),
    ("percent stays raw", "/home/u/100%", "/home/u/100%"),
    ("ESC", "/home/u/e\x1bx", "/home/u/e?x"),
    ("BEL", "/home/u/b\x07x", "/home/u/b?x"),
    ("CAN", "/home/u/c\x18x", "/home/u/c?x"),
    ("SUB", "/home/u/s\x1ax", "/home/u/s?x"),
    ("newline", "/home/u/n\nx", "/home/u/n?x"),
    ("DEL", "/home/u/d\x7fx", "/home/u/d?x"),
    ("multiple control bytes", "/home/u/e\x1bx\x07y\x18z\x1aw\nv\x7fq", "/home/u/e?x?y?z?w?v?q"),
    ("non-ASCII", "/home/u/\u00e9t\u00e9/\u65e5\u672c", "/home/u/\u00e9t\u00e9/\u65e5\u672c"),
]

OSC7_CALL = f'. "$ROOST_TEST_SCRIPT"; HOSTNAME={HOST}; HOST={HOST}; PWD=$ROOST_TEST_PWD; __roost_osc7'
TITLE_CALL = '. "$ROOST_TEST_SCRIPT"; PWD=$ROOST_TEST_PWD; __roost_title'

USER_TITLE_SCRIPT = (
    "PWD=/tmp/wherever\n"
    "__roost_title() { printf 'USER-DEFINED %s' \"$PWD\"; }\n"
    '. "$ROOST_TEST_SCRIPT"\n'
    "__roost_title\n"
)


def _function_text(text: str, name: str) -> str:
    """The full `name() { ... }` definition, single- or multi-line."""
    marker = f"{name}() {{"
    starts = [i for i in range(len(text)) if text.startswith(marker, i)]
    if len(starts) != 1:
        raise AssertionError(f"expected exactly one {name} definition, found {len(starts)}")
    idx = starts[0]
    line_end = text.index("\n", idx) + 1
    first_line = text[idx:line_end]
    if first_line.count("{") == first_line.count("}"):
        return first_line
    pos = line_end
    while True:
        next_end = text.index("\n", pos) + 1
        if text[pos:next_end] == "}\n":
            return text[idx:next_end]
        pos = next_end


def _guarded_block(text: str, name: str) -> str:
    """`name`'s definition plus the `if ... then` / `fi` redefinition
    guard around it, so a dropped or edited guard fails the compare too."""
    fn_text = _function_text(text, name)
    fn_idx = text.index(fn_text)
    if_start = text.rfind("\nif ", 0, fn_idx)
    if if_start == -1:
        raise AssertionError(f"no guard 'if' found before {name}")
    if_start += 1
    after = fn_idx + len(fn_text)
    if not text[after:].startswith("fi\n"):
        raise AssertionError(f"no matching 'fi' found after {name}")
    return text[if_start : after + len("fi\n")]


class ShellIntegrationEmitterTest(unittest.TestCase):
    def _shell(self, name: str, override: str) -> str:
        chosen = os.environ.get(override)
        path = shutil.which(chosen or name)
        if path:
            return path
        message = f"{name} not found ({override}={chosen!r}); needed to run the {name} shell integration"
        if chosen or os.environ.get("CI"):
            self.fail(message)
        self.skipTest(message)

    def _run(
        self,
        script: Path,
        call: str,
        features: str,
        pwd: str,
        locale: str,
    ) -> subprocess.CompletedProcess[bytes]:
        name, override, flags = SHELLS[script.suffix]
        argv = [self._shell(name, override), *flags, call]
        with tempfile.TemporaryDirectory(prefix="roost-unit-shell-") as home:
            env = {
                "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                "HOME": home,
                "ROOST_TAB_ID": "1",
                "ROOST_SHELL_FEATURES": features,
                "ROOST_TEST_SCRIPT": str(script),
                "LC_ALL": locale,
                "ROOST_TEST_PWD": pwd,
            }
            # A new session keeps the interactive shell off the caller's
            # terminal, which it would otherwise grab.
            return subprocess.run(
                argv,
                env=env,
                stdin=subprocess.DEVNULL,
                capture_output=True,
                start_new_session=True,
                timeout=30,
            )

    def _assert_osc7_emits(self, script: Path) -> None:
        for locale in LOCALES:
            for label, pwd, wire in OSC7_CASES:
                with self.subTest(script=script.name, locale=locale, case=label):
                    run = self._run(script, OSC7_CALL, "cwd", pwd, locale)
                    self.assertEqual(
                        run.stdout,
                        f"\x1b]7;file://{HOST}{wire}\x1b\\".encode(),
                        f"exit {run.returncode}, stderr: {run.stderr.decode(errors='replace')}",
                    )

    def _assert_title_emits(self, script: Path) -> None:
        for locale in LOCALES:
            for label, pwd, wire in TITLE_CASES:
                with self.subTest(script=script.name, locale=locale, case=label):
                    run = self._run(script, TITLE_CALL, "title", pwd, locale)
                    self.assertEqual(
                        run.stdout,
                        f"\x1b]0;{wire}\x1b\\".encode(),
                        f"exit {run.returncode}, stderr: {run.stderr.decode(errors='replace')}",
                    )

    def test_rust_bash_osc7(self) -> None:
        self._assert_osc7_emits(RUST_DIR / "roost.bash")

    def test_mac_bash_osc7(self) -> None:
        self._assert_osc7_emits(MAC_DIR / "roost.bash")

    def test_rust_zsh_osc7(self) -> None:
        self._assert_osc7_emits(RUST_DIR / "roost.zsh")

    def test_mac_zsh_osc7(self) -> None:
        self._assert_osc7_emits(MAC_DIR / "roost.zsh")

    def test_rust_bash_title(self) -> None:
        self._assert_title_emits(RUST_DIR / "roost.bash")

    def test_mac_bash_title(self) -> None:
        self._assert_title_emits(MAC_DIR / "roost.bash")

    def test_rust_zsh_title(self) -> None:
        self._assert_title_emits(RUST_DIR / "roost.zsh")

    def test_mac_zsh_title(self) -> None:
        self._assert_title_emits(MAC_DIR / "roost.zsh")

    def test_bash_bodies_match(self) -> None:
        rust = (RUST_DIR / "roost.bash").read_text(encoding="utf-8")
        mac = (MAC_DIR / "roost.bash").read_text(encoding="utf-8")
        for name in BASH_SHARED_FUNCTIONS:
            with self.subTest(function=name):
                self.assertEqual(_guarded_block(rust, name), _guarded_block(mac, name))

    def test_zsh_bodies_match(self) -> None:
        rust = (RUST_DIR / "roost.zsh").read_text(encoding="utf-8")
        mac = (MAC_DIR / "roost.zsh").read_text(encoding="utf-8")
        for name in ZSH_SHARED_FUNCTIONS:
            with self.subTest(function=name):
                self.assertEqual(_guarded_block(rust, name), _guarded_block(mac, name))

    def test_osc7_bash_and_zsh_bodies_match(self) -> None:
        rust_bash = (RUST_DIR / "roost.bash").read_text(encoding="utf-8")
        rust_zsh = (RUST_DIR / "roost.zsh").read_text(encoding="utf-8")
        bash_body = _function_text(rust_bash, "__roost_osc7")
        zsh_body = _function_text(rust_zsh, "__roost_osc7")
        self.assertEqual(bash_body.replace('"${HOSTNAME:-}"', '"${HOST}"'), zsh_body)

    def _assert_user_title_survives(self, script: Path) -> None:
        name, override, flags = SHELLS[script.suffix]
        argv = [self._shell(name, override), *flags, USER_TITLE_SCRIPT]
        with tempfile.TemporaryDirectory(prefix="roost-unit-guard-") as home:
            env = {
                "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                "HOME": home,
                "ROOST_TAB_ID": "1",
                "ROOST_SHELL_FEATURES": "title",
                "ROOST_TEST_SCRIPT": str(script),
                "LC_ALL": "C",
            }
            run = subprocess.run(
                argv,
                env=env,
                stdin=subprocess.DEVNULL,
                capture_output=True,
                start_new_session=True,
                timeout=30,
            )
            self.assertEqual(
                run.stdout,
                b"USER-DEFINED /tmp/wherever",
                f"exit {run.returncode}, stderr: {run.stderr.decode(errors='replace')}",
            )

    def test_mac_bash_user_title_survives(self) -> None:
        self._assert_user_title_survives(MAC_DIR / "roost.bash")

    def test_mac_zsh_user_title_survives(self) -> None:
        self._assert_user_title_survives(MAC_DIR / "roost.zsh")


if __name__ == "__main__":
    unittest.main()
