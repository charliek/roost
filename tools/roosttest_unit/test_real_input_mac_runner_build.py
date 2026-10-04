"""`tools/input/mac/test-runner/build.sh` never replaces an installed TCC
anchor (plan 074 §D7): a rebuild voids the grants, so there is no override.

Run against a throwaway HOME with `uname`, `clang` and `codesign` faked on
PATH, so it runs on any OS and builds nothing real.
"""

from __future__ import annotations

import os
import shutil
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

BUILD_SH = Path(__file__).resolve().parents[1] / "input" / "mac" / "test-runner" / "build.sh"

FAKES = {
    "uname": 'echo Darwin\n',
    # Writes whatever `-o` names, and logs the call.
    "clang": 'echo "clang $*" >> "$FAKE_LOG"\n'
             'while [ "$#" -gt 0 ]; do [ "$1" = -o ] && { echo built > "$2"; }; shift; done\n',
    "codesign": 'echo "codesign $*" >> "$FAKE_LOG"\necho "Identifier=ai.stridelabs.roost.test-runner"\n',
}


class RunnerBuildTests(unittest.TestCase):
    def setUp(self) -> None:
        self.root = Path(tempfile.mkdtemp(prefix="ri-build-"))
        self.addCleanup(shutil.rmtree, self.root, True)
        self.home = self.root / "home"
        self.home.mkdir()
        fakes = self.root / "bin"
        fakes.mkdir()
        for name, body in FAKES.items():
            path = fakes / name
            path.write_text("#!/bin/sh\n" + body)
            path.chmod(path.stat().st_mode | stat.S_IXUSR)
        self.log = self.root / "calls"
        self.log.touch()
        self.env = {
            "HOME": str(self.home),
            "PATH": f"{fakes}:/usr/bin:/bin",
            "FAKE_LOG": str(self.log),
        }
        self.app = self.home / "Applications" / "Roost Test Runner.app"
        self.probe = self.home / "roost-harness" / "tcc-probe"

    def build(self, *args: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["bash", str(BUILD_SH), *args],
            env=self.env, capture_output=True, text=True, timeout=60, check=False,
        )

    def install_existing(self) -> None:
        (self.app / "Contents").mkdir(parents=True)
        (self.app / "Contents" / "granted").write_text("the copy TCC knows\n")

    def test_an_installed_runner_is_never_replaced(self) -> None:
        self.install_existing()
        for args in ((), ("--force",)):
            with self.subTest(args=args):
                result = self.build(*args)
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertEqual(
                    (self.app / "Contents" / "granted").read_text(), "the copy TCC knows\n"
                )
                self.assertEqual(self.log.read_text(), "", "nothing may be built or signed")
                self.assertFalse(self.probe.exists())

    def test_a_fresh_install_builds_the_runner_and_the_probe(self) -> None:
        result = self.build()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            (self.app / "Contents" / "MacOS" / "roost-test-runner").read_text(), "built\n"
        )
        self.assertEqual(self.probe.read_text(), "built\n")

    def test_a_fresh_install_keeps_an_existing_probe(self) -> None:
        self.probe.parent.mkdir(parents=True)
        self.probe.write_text("someone's probe\n")
        result = self.build()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.probe.read_text(), "someone's probe\n")


if __name__ == "__main__":
    unittest.main()
