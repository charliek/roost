"""Plan 072 D13: the verdicts of `old_session/fetch.sh` that must never
wait on GitHub.

Offline by construction. gh is pointed at a closed loopback port with no
token (`GH_HOST`), or replaced outright by a stub on `PATH`, and the cache
is an empty temp dir — so what is pinned is the
script's own policy: an unreachable GitHub with nothing cached is a skip,
a binary that does not match its `.sha256` is a failure, and so is a
release with no `.sha256` beside its binary.
"""

from __future__ import annotations

import hashlib
import os
import platform
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

FETCH = Path(__file__).resolve().parents[1] / "roosttest" / "old_session" / "fetch.sh"
#: Nothing listens on the discard port, so gh fails at connect, locally.
CLOSED_PORT = "127.0.0.1:9"
_GH_ENV = (
    "GH_HOST",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "ROOST_OLD_SESSION_VERSION",
)


def asset(version: str) -> str:
    arch = "arm64" if platform.machine() in ("aarch64", "arm64") else "amd64"
    return f"roost-session-{version}-linux-{arch}"


@unittest.skipUnless(sys.platform.startswith("linux"), "releases ship roost-session for Linux only")
class FetchVerdicts(unittest.TestCase):
    def setUp(self) -> None:
        self.scratch = Path(tempfile.mkdtemp(prefix="roost-fetch-"))
        self.addCleanup(shutil.rmtree, self.scratch, ignore_errors=True)
        self.cache = self.scratch / "cache"

    def fetch(self, *args: str, path: str | None = None) -> subprocess.CompletedProcess:
        env = {k: v for k, v in os.environ.items() if k not in _GH_ENV}
        env["ROOST_OLD_SESSION_CACHE"] = str(self.cache)
        env["GH_HOST"] = CLOSED_PORT
        if path is not None:
            env["PATH"] = path
        return subprocess.run(
            ["bash", str(FETCH), *args], env=env, capture_output=True, text=True, timeout=120
        )

    def test_an_unreachable_github_and_an_empty_cache_skip_with_a_warning(self) -> None:
        for which in ("0.0.20", "latest"):
            with self.subTest(which=which):
                result = self.fetch(which)
                self.assertEqual(result.returncode, 0, result.stderr)
                lines = result.stdout.splitlines()
                self.assertTrue(any(line.startswith("::warning::") for line in lines), lines)
                self.assertTrue(any(line.startswith("skip=") for line in lines), lines)
                self.assertFalse(any(line.startswith("binary=") for line in lines), lines)
                self.assertEqual(list(self.scratch.rglob("roost-session")), [])

    def test_a_cached_binary_is_served_offline_only_while_it_matches_its_sha256(self) -> None:
        cached = self.cache / "0.0.20"
        cached.mkdir(parents=True)
        binary = cached / "roost-session"
        binary.write_bytes(b"#!/bin/sh\n")
        digest = hashlib.sha256(binary.read_bytes()).hexdigest()
        (cached / "roost-session.sha256").write_text(f"{digest}  {asset('0.0.20')}\n")

        served = self.fetch("0.0.20")
        self.assertEqual(served.returncode, 0, served.stderr)
        self.assertIn(f"binary={binary}", served.stdout.splitlines())

        binary.write_bytes(b"#!/bin/sh\necho tampered\n")
        tampered = self.fetch("0.0.20")
        self.assertNotEqual(tampered.returncode, 0, tampered.stdout)
        self.assertIn("does not match its .sha256", tampered.stderr)
        self.assertNotIn("binary=", tampered.stdout)
        self.assertNotIn("skip=", tampered.stdout)

    def test_a_release_without_a_sha256_fails(self) -> None:
        stub_dir = self.scratch / "bin"
        stub_dir.mkdir()
        stub = stub_dir / "gh"
        stub.write_text(
            "#!/bin/sh\n"
            'if [ "$1 $2" = "release view" ]; then\n'
            f"  echo {asset('0.0.20')}\n"
            "  exit 0\n"
            "fi\n"
            'echo "stub gh: unexpected $*" >&2\n'
            "exit 1\n"
        )
        stub.chmod(0o755)

        result = self.fetch("0.0.20", path=f"{stub_dir}{os.pathsep}{os.environ['PATH']}")
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn(f"no {asset('0.0.20')}.sha256", result.stderr)
        self.assertNotIn("skip=", result.stdout)


if __name__ == "__main__":
    unittest.main()
