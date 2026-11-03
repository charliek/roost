"""tools/release/check_assets.py: the asset set `publish-release` refuses to
publish without. Runs the real script for the exit-code and stdin contract,
and its functions for the rules."""

from __future__ import annotations

import importlib.util
import subprocess
import sys
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
TOOL = REPO_ROOT / "tools" / "release" / "check_assets.py"
MIB = 1024 * 1024


def load_tool():
    spec = importlib.util.spec_from_file_location("release_check_assets", TOOL)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def full_set(ver: str, deb_ver: str) -> list[tuple[str, int]]:
    """A complete draft, as GitHub stores the names."""
    assets = [
        (f"roost_{deb_ver}_amd64.deb", 9 * MIB),
        (f"roost_{deb_ver}_arm64.deb", 9 * MIB),
        (f"Roost-{ver}.dmg", 40 * MIB),
    ]
    for platform in ("linux-amd64", "linux-arm64", "darwin-arm64"):
        name = f"roost-session-{ver}-{platform}"
        assets.append((name, 12 * MIB))
        assets.append((f"{name}.sha256", 64 + 2 + len(name) + 1))
    return assets


class CheckAssetsTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tool = load_tool()

    def run_tool(self, tag: str, assets: list[tuple[str, int]]) -> subprocess.CompletedProcess:
        stdin = "".join(f"{name}\t{size}\n" for name, size in assets)
        return subprocess.run(
            [sys.executable, str(TOOL), tag], input=stdin, capture_output=True, text=True
        )

    def test_a_complete_stable_draft_passes(self):
        result = self.run_tool("v0.1.0", full_set("0.1.0", "0.1.0"))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotIn("::error::", result.stdout)

    def test_a_complete_prerelease_draft_passes_under_its_sanitized_names(self):
        assets = full_set("0.1.0-rc1", "0.1.0.rc1")
        self.assertIn(("roost_0.1.0.rc1_amd64.deb", 9 * MIB), assets)
        result = self.run_tool("v0.1.0-rc1", assets)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_the_debian_tilde_replaces_only_the_first_hyphen(self):
        names = self.tool.expected_assets("v1.2.3-rc-2")
        self.assertIn("roost_1.2.3.rc-2_amd64.deb", names)
        self.assertIn("roost-session-1.2.3-rc-2-linux-amd64", names)

    def test_exactly_nine_assets_and_no_iced_dmg(self):
        names = self.tool.expected_assets("v0.1.0")
        self.assertEqual(len(names), 9)
        self.assertIn("Roost-0.1.0.dmg", names)
        self.assertFalse(any("Iced" in name for name in names), names)

    def test_a_missing_asset_fails_and_is_named(self):
        for missing in full_set("0.1.0", "0.1.0"):
            with self.subTest(missing=missing[0]):
                assets = [a for a in full_set("0.1.0", "0.1.0") if a != missing]
                result = self.run_tool("v0.1.0", assets)
                self.assertEqual(result.returncode, 1)
                self.assertIn(f"exactly one asset named {missing[0]}", result.stdout)

    def test_a_duplicate_asset_fails(self):
        assets = full_set("0.1.0", "0.1.0") + [("Roost-0.1.0.dmg", 40 * MIB)]
        result = self.run_tool("v0.1.0", assets)
        self.assertEqual(result.returncode, 1)
        self.assertIn("exactly one asset named Roost-0.1.0.dmg on v0.1.0, found 2", result.stdout)

    def test_an_undersized_artifact_fails(self):
        assets = [
            (name, MIB - 1 if name == "roost_0.1.0_arm64.deb" else size)
            for name, size in full_set("0.1.0", "0.1.0")
        ]
        result = self.run_tool("v0.1.0", assets)
        self.assertEqual(result.returncode, 1)
        self.assertIn("roost_0.1.0_arm64.deb on v0.1.0 is only 1048575 bytes", result.stdout)

    def test_a_sidecar_floor_is_its_readers_minimum_record(self):
        name = "roost-session-0.1.0-darwin-arm64"
        floor = 64 + 1 + len(name)
        for size, ok in ((floor, True), (floor - 1, False), (64, False)):
            with self.subTest(size=size):
                assets = [
                    (n, size if n == f"{name}.sha256" else s)
                    for n, s in full_set("0.1.0", "0.1.0")
                ]
                result = self.run_tool("v0.1.0", assets)
                self.assertEqual(result.returncode, 0 if ok else 1, result.stdout)

    def test_an_unexpected_asset_fails(self):
        for stray in ("Roost-Iced-0.1.0.dmg", "roost_0.0.27_amd64.deb"):
            with self.subTest(stray=stray):
                result = self.run_tool("v0.1.0", full_set("0.1.0", "0.1.0") + [(stray, 9 * MIB)])
                self.assertEqual(result.returncode, 1)
                self.assertIn(f"unexpected asset {stray} on v0.1.0", result.stdout)

    def test_an_unsanitized_prerelease_name_is_unexpected_and_missing(self):
        assets = [
            ("roost_0.1.0~rc1_amd64.deb" if n == "roost_0.1.0.rc1_amd64.deb" else n, s)
            for n, s in full_set("0.1.0-rc1", "0.1.0.rc1")
        ]
        result = self.run_tool("v0.1.0-rc1", assets)
        self.assertEqual(result.returncode, 1)
        self.assertIn("unexpected asset roost_0.1.0~rc1_amd64.deb", result.stdout)
        self.assertIn("exactly one asset named roost_0.1.0.rc1_amd64.deb", result.stdout)

    def test_a_malformed_line_fails_rather_than_reading_as_no_asset(self):
        result = subprocess.run(
            [sys.executable, str(TOOL), "v0.1.0"],
            input="Roost-0.1.0.dmg 40000000\n",
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("not `name<TAB>size`", result.stdout)

    def test_publish_release_runs_this_script(self):
        release = (REPO_ROOT / ".github" / "workflows" / "release.yml").read_text()
        job = release.split("\n  publish-release:\n", 1)[1].split("\n  appcast:\n", 1)[0]
        self.assertIn("python3 tools/release/check_assets.py", job)
        self.assertIn("    needs: [linux, mac]\n", job)


if __name__ == "__main__":
    unittest.main()
