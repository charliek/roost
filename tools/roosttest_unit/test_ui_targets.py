"""Fast unit coverage for the functional harness target contract.

Kept outside ``tools/roosttest`` so pytest's E2E session fixture does not
launch a UI merely to test pure path and capability metadata.
"""

from __future__ import annotations

import os
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

ROOSTTEST_DIR = Path(__file__).resolve().parents[1] / "roosttest"
sys.path.insert(0, str(ROOSTTEST_DIR))

import ui  # noqa: E402


class TargetContractTests(unittest.TestCase):
    def test_target_table_has_three_explicit_profiles(self) -> None:
        self.assertEqual(ui.TARGETS, ("mac", "gtk", "iced"))
        self.assertEqual(ui.TARGET_SPECS["iced"].rust_package, "roost-iced")
        self.assertEqual(ui.TARGET_SPECS["iced"].binary_name, "roost-iced")
        self.assertFalse(ui.TARGET_SPECS["iced"].scans_gtk_criticals)
        self.assertTrue(ui.TARGET_SPECS["mac"].isolates_user_defaults)

    @patch("ui.Path.home", return_value=Path("/Users/tester"))
    @patch("ui.platform.system", return_value="Darwin")
    def test_macos_paths_are_pairwise_distinct(self, _system, _home) -> None:
        paths = {target: ui.socket_path(target) for target in ui.TARGETS}
        self.assertEqual(paths["iced"], Path("/Users/tester/Library/Caches/Roost-iced/roost.sock"))
        self.assertEqual(len(set(paths.values())), 3)

    @patch.dict(os.environ, {"XDG_RUNTIME_DIR": "/run/user/1000"}, clear=False)
    @patch("ui.platform.system", return_value="Linux")
    def test_linux_gtk_and_iced_paths_are_distinct(self, _system) -> None:
        self.assertEqual(ui.socket_path("gtk"), Path("/run/user/1000/roost/roost.sock"))
        self.assertEqual(
            ui.socket_path("iced"), Path("/run/user/1000/roost-iced/roost.sock")
        )

    def test_unknown_target_names_all_candidates(self) -> None:
        with self.assertRaisesRegex(ValueError, r"want mac\|gtk\|iced"):
            ui.socket_path("other")

    @patch.dict(os.environ, {"XDG_RUNTIME_DIR": "relative/runtime"}, clear=False)
    @patch("ui.os.getuid", return_value=1234)
    @patch("ui.platform.system", return_value="Linux")
    def test_relative_xdg_runtime_dir_uses_safe_fallback(
        self, _system, _uid
    ) -> None:
        self.assertEqual(
            ui.socket_path("iced"), Path("/tmp/roost-iced-1234/roost.sock")
        )


if __name__ == "__main__":
    unittest.main()
