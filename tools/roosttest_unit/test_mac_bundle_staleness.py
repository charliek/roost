"""Unit coverage for the mac bundle-staleness check in `tools/roosttest/ui.py`
(plan 066 C1 / #493).

No real Swift build or macOS here: `_ensure_mac_bundle` takes an injectable
`runner` in place of `subprocess.run`, and the staleness check is pure
mtime comparison over a directory tree built with `os.utime`.
"""

from __future__ import annotations

import io
import os
import shutil
import sys
import tempfile
import time
import unittest
import warnings
from contextlib import redirect_stderr
from pathlib import Path
from unittest.mock import Mock, patch

ROOSTTEST_DIR = Path(__file__).resolve().parents[1] / "roosttest"
sys.path.insert(0, str(ROOSTTEST_DIR))

import ui  # noqa: E402


def _make_bundle(root: Path, binary_mtime: float) -> Path:
    app = root / "Roost.app"
    macos_dir = app / "Contents" / "MacOS"
    macos_dir.mkdir(parents=True)
    binary = macos_dir / "Roost"
    binary.write_text("#!/bin/sh\n")
    os.utime(binary, (binary_mtime, binary_mtime))
    return app


def _make_mac_dir(root: Path, source_mtime: float) -> Path:
    mac_dir = root / "mac"
    (mac_dir / "Sources" / "Roost").mkdir(parents=True)
    src = mac_dir / "Sources" / "Roost" / "App.swift"
    src.write_text("// swift\n")
    os.utime(src, (source_mtime, source_mtime))
    # A stale file left in mac/build (and .build) must never count — the
    # bundle step's own output tree would otherwise make itself look stale.
    for build_dir in ("build", ".build"):
        junk_dir = mac_dir / build_dir
        junk_dir.mkdir()
        junk = junk_dir / "junk"
        junk.write_text("junk\n")
        os.utime(junk, (source_mtime + 10_000, source_mtime + 10_000))
    return mac_dir


class MacBundleStalenessTests(unittest.TestCase):
    def setUp(self) -> None:
        self._root = Path(tempfile.mkdtemp(prefix="roost-unit-mac-bundle-"))
        self.addCleanup(shutil.rmtree, self._root, True)
        patcher = patch("ui._MAC_BUNDLED_ONCE", False)
        patcher.start()
        self.addCleanup(patcher.stop)

    def test_default_run_invokes_the_rebuild_runner(self) -> None:
        base = time.time() - 100
        app = _make_bundle(self._root, base)
        mac_dir = _make_mac_dir(self._root, base)
        runner = Mock()
        with patch.dict(os.environ, {}, clear=False):
            os.environ.pop("ROOST_MAC_NO_BUNDLE", None)
            ui._ensure_mac_bundle(app, mac_dir, runner=runner)
        runner.assert_called_once_with(
            ["./scripts/bundle.sh", "debug"], cwd=mac_dir, check=True
        )

    def test_opt_out_skips_the_rebuild_runner(self) -> None:
        base = time.time() - 100
        app = _make_bundle(self._root, base)
        mac_dir = _make_mac_dir(self._root, base)
        runner = Mock()
        with patch.dict(os.environ, {"ROOST_MAC_NO_BUNDLE": "1"}):
            ui._ensure_mac_bundle(app, mac_dir, runner=runner)
        runner.assert_not_called()

    def test_opt_out_without_a_bundle_fails_loudly(self) -> None:
        mac_dir = _make_mac_dir(self._root, time.time())
        missing_app = self._root / "Roost.app"
        runner = Mock()
        with patch.dict(os.environ, {"ROOST_MAC_NO_BUNDLE": "1"}):
            with self.assertRaisesRegex(FileNotFoundError, "ROOST_MAC_NO_BUNDLE=1"):
                ui._ensure_mac_bundle(missing_app, mac_dir, runner=runner)
        runner.assert_not_called()

    def test_stale_bundle_warns_when_sources_are_newer(self) -> None:
        app = _make_bundle(self._root, time.time() - 1000)
        mac_dir = _make_mac_dir(self._root, time.time())
        runner = Mock()
        with patch.dict(os.environ, {"ROOST_MAC_NO_BUNDLE": "1"}):
            with redirect_stderr(io.StringIO()):
                with self.assertWarnsRegex(
                    UserWarning, "stale Roost.app: sources are newer than the bundle"
                ):
                    ui._ensure_mac_bundle(app, mac_dir, runner=runner)

    def test_fresh_bundle_does_not_warn(self) -> None:
        app = _make_bundle(self._root, time.time())
        mac_dir = _make_mac_dir(self._root, time.time() - 1000)
        runner = Mock()
        buf = io.StringIO()
        with patch.dict(os.environ, {"ROOST_MAC_NO_BUNDLE": "1"}):
            with redirect_stderr(buf), warnings.catch_warnings(record=True) as caught:
                warnings.simplefilter("always")
                ui._ensure_mac_bundle(app, mac_dir, runner=runner)
        self.assertEqual([str(w.message) for w in caught], [])
        self.assertIn("mac bundle binary mtime=", buf.getvalue())

    def test_at_most_once_per_process(self) -> None:
        app = _make_bundle(self._root, time.time())
        mac_dir = _make_mac_dir(self._root, time.time() - 1000)
        runner = Mock()
        with patch.dict(os.environ, {}, clear=False):
            os.environ.pop("ROOST_MAC_NO_BUNDLE", None)
            ui._ensure_mac_bundle(app, mac_dir, runner=runner)
            ui._ensure_mac_bundle(app, mac_dir, runner=runner)
        runner.assert_called_once()


if __name__ == "__main__":
    unittest.main()
