"""Regression coverage for tools/screenshot/lib.sh's ut_socket_for().

`ut_socket_for` is a bash reimplementation of roost-ipc's BundleProfile
socket resolver (crates/roost-ipc/src/paths.rs — the non-macOS
`resolve_paths` + `xdg_runtime_dir`) for the screenshot/e2e test harness.
It must stay byte-identical to that resolver or tooling silently dials
the wrong socket.

Runs the real `ut_socket_for` bash function in a subprocess (not a
Python port of its logic) with a stub `uname` prepended onto PATH, so
the non-Darwin code path is exercised deterministically on every host
running this suite — including a developer's Mac.
"""

from __future__ import annotations

import os
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

LIB_SH = Path(__file__).resolve().parents[1] / "screenshot" / "lib.sh"

UID = os.getuid()


def _write_fake_uname(bin_dir: Path, kernel_name: str) -> None:
    """Write a stub `uname` that always reports `kernel_name`.

    lib.sh's gtk/iced arms branch on `uname -s`. Faking it forces the
    non-Darwin branch deterministically regardless of the host OS,
    instead of relying on (or re-deriving) the runner's real platform.
    """
    script = bin_dir / "uname"
    script.write_text(f'#!/bin/sh\necho "{kernel_name}"\n')
    script.chmod(script.stat().st_mode | stat.S_IEXEC | stat.S_IXGRP | stat.S_IXOTH)


def _ut_socket_for(target: str, *, xdg_runtime_dir: "str | None") -> str:
    """Source lib.sh in bash and return `ut_socket_for(target)`'s stdout."""
    with tempfile.TemporaryDirectory() as fake_bin:
        fake_bin_path = Path(fake_bin)
        _write_fake_uname(fake_bin_path, "Linux")

        env = dict(os.environ)
        env["PATH"] = f"{fake_bin_path}:{env.get('PATH', '')}"
        if xdg_runtime_dir is None:
            env.pop("XDG_RUNTIME_DIR", None)
        else:
            env["XDG_RUNTIME_DIR"] = xdg_runtime_dir

        result = subprocess.run(
            ["bash", "-c", 'set -euo pipefail; . "$1"; ut_socket_for "$2"', "_", str(LIB_SH), target],
            env=env,
            capture_output=True,
            text=True,
        )
    if result.returncode != 0:
        raise RuntimeError(f"ut_socket_for {target} failed (rc={result.returncode}): {result.stderr}")
    return result.stdout.strip()


class UtSocketForGtkTests(unittest.TestCase):
    """Parity with roost-ipc's non-macOS resolve_paths() for BundleProfileKind::Gtk."""

    def test_absolute_xdg_runtime_dir_is_used(self) -> None:
        self.assertEqual(
            _ut_socket_for("gtk", xdg_runtime_dir="/run/user/1000"),
            "/run/user/1000/roost/roost.sock",
        )

    def test_unset_xdg_runtime_dir_falls_back(self) -> None:
        self.assertEqual(
            _ut_socket_for("gtk", xdg_runtime_dir=None),
            f"/tmp/roost-{UID}/roost.sock",
        )

    def test_empty_xdg_runtime_dir_falls_back(self) -> None:
        self.assertEqual(
            _ut_socket_for("gtk", xdg_runtime_dir=""),
            f"/tmp/roost-{UID}/roost.sock",
        )

    def test_relative_xdg_runtime_dir_falls_back(self) -> None:
        self.assertEqual(
            _ut_socket_for("gtk", xdg_runtime_dir="relative/path"),
            f"/tmp/roost-{UID}/roost.sock",
        )


class UtSocketForIcedTests(unittest.TestCase):
    """Same XDG_RUNTIME_DIR parity, for BundleProfileKind::Iced's isolated namespace."""

    def test_absolute_xdg_runtime_dir_is_used(self) -> None:
        self.assertEqual(
            _ut_socket_for("iced", xdg_runtime_dir="/run/user/1000"),
            "/run/user/1000/roost-iced/roost.sock",
        )

    def test_unset_xdg_runtime_dir_falls_back(self) -> None:
        self.assertEqual(
            _ut_socket_for("iced", xdg_runtime_dir=None),
            f"/tmp/roost-iced-{UID}/roost.sock",
        )

    def test_empty_xdg_runtime_dir_falls_back(self) -> None:
        self.assertEqual(
            _ut_socket_for("iced", xdg_runtime_dir=""),
            f"/tmp/roost-iced-{UID}/roost.sock",
        )

    def test_relative_xdg_runtime_dir_falls_back(self) -> None:
        self.assertEqual(
            _ut_socket_for("iced", xdg_runtime_dir="relative/path"),
            f"/tmp/roost-iced-{UID}/roost.sock",
        )


if __name__ == "__main__":
    unittest.main()
