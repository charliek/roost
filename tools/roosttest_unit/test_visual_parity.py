from __future__ import annotations

import sys
import tempfile
import unittest
from unittest import mock
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "tools" / "screenshot"))

import parity  # noqa: E402


def image(width: int, height: int, fills: list[tuple[int, int, int]]) -> parity.Image:
    return width, height, 3, bytes(channel for color in fills for channel in color)


class PixelMeasurementTests(unittest.TestCase):
    def test_counts_runs_and_components_are_geometry_not_text_based(self):
        black = (0, 0, 0)
        blue = parity.LIFECYCLE_COLORS["working"]
        fills = [black] * 60
        for y in range(1, 5):
            for x in range(2, 6):
                fills[y * 10 + x] = blue
        shot = image(10, 6, fills)

        self.assertEqual(parity.pixel(shot, 2, 1), blue)
        self.assertEqual(parity.count_color(shot, blue), 16)
        self.assertEqual(parity.first_vertical_run(shot, 2, blue, minimum=4), 1)
        self.assertEqual(parity.first_horizontal_run(shot, 2, blue, minimum=4), 2)
        self.assertEqual(
            parity.color_components(shot, blue, (0, 0, 10, 6), minimum_side=4),
            [{"left": 2, "top": 1, "right": 5, "bottom": 4, "pixels": 16}],
        )

    def test_environment_key_keeps_renderer_and_display_distinct(self):
        base = {
            "target": "iced",
            "os": "linux",
            "display_backend": "x11",
            "renderer": "wgpu",
            "scale": 1,
        }
        tiny = {**base, "renderer": "tiny-skia"}
        wayland = {**base, "display_backend": "wayland"}
        self.assertEqual(parity.environment_key(base), "iced-linux-x11-wgpu-1")
        keys = {parity.environment_key(item) for item in (base, tiny, wayland)}
        self.assertEqual(len(keys), 3)

    def test_target_backend_override_wins_over_ambient_display(self):
        environment = {
            "GDK_BACKEND": "x11",
            "WAYLAND_DISPLAY": "wayland-1",
        }
        with tempfile.NamedTemporaryFile() as executable:
            environment["ROOST_PARITY_EXECUTABLE"] = executable.name
            with (
                mock.patch.dict(parity.os.environ, environment, clear=True),
                mock.patch.object(parity.platform, "system", return_value="Linux"),
            ):
                metadata = parity.environment_metadata("gtk", "run", "commit")
        self.assertEqual(metadata["display_backend"], "x11")
        self.assertEqual(
            metadata["backend_environment"],
            {"GDK_BACKEND": "x11", "WAYLAND_DISPLAY": "wayland-1"},
        )

    def test_darwin_ignores_ambient_unix_display_variables(self):
        with tempfile.NamedTemporaryFile() as executable:
            environment = {
                "DISPLAY": ":99",
                "WAYLAND_DISPLAY": "wayland-9",
                "WINIT_UNIX_BACKEND": "x11",
                "ROOST_PARITY_EXECUTABLE": executable.name,
            }
            with (
                mock.patch.dict(parity.os.environ, environment, clear=True),
                mock.patch.object(parity.platform, "system", return_value="Darwin"),
            ):
                metadata = parity.environment_metadata("iced", "run", "commit")
        self.assertEqual(metadata["display_backend"], "native")

    def test_central_digest_ignores_edges_but_detects_palette_region(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "shot.png"
            # Exercise the crop math without depending on a PNG encoder.
            shot = image(8, 10, [(0, 0, 0)] * 80)
            with mock.patch.object(parity.pngtool, "load", return_value=shot):
                first = parity.central_region_digest(path)
            fills = [(0, 0, 0)] * 80
            fills[5 * 8 + 4] = (255, 255, 255)
            with mock.patch.object(parity.pngtool, "load", return_value=image(8, 10, fills)):
                second = parity.central_region_digest(path)
            self.assertNotEqual(first, second)

    def test_atomic_json_replaces_complete_documents(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "measurements.json"
            parity.atomic_json(path, {"revision": 1})
            parity.atomic_json(path, {"revision": 2})
            self.assertEqual(path.read_text(), '{\n  "revision": 2\n}\n')
            self.assertEqual(list(path.parent.glob("tmp*")), [])


class ManifestTests(unittest.TestCase):
    def document(self, renderer: str = "wgpu") -> dict:
        metadata = {
            "schema_version": parity.SCHEMA_VERSION,
            "scenario": parity.SCENARIO,
            "target": "iced",
            "os": "linux",
            "display_backend": "x11",
            "renderer": renderer,
            "scale": 1,
            "commit": "abc123",
            "run_id": "run-7",
            "source_dirty": True,
            "executable": {"path": "/tmp/roost-iced", "sha256": "c" * 64},
        }
        return {
            "metadata": metadata,
            "shell": {
                "png": "shell.png",
                "sha256": "a" * 64,
                "width": 1100,
                "height": 700,
                "sidebar_sample": [17, 17, 17],
                "terminal_top": 44,
            },
            "palette": {
                "available": True,
                "png": "palette.png",
                "sha256": "b" * 64,
            },
        }

    def test_manifest_names_run_commit_and_environment(self):
        rendered = parity.format_manifest([self.document()], "run-7", "abc123")
        self.assertIn("Run: `run-7`", rendered)
        self.assertIn("Commit: `abc123`", rendered)
        self.assertIn("iced | linux | x11 | wgpu", rendered)
        self.assertIn("[shell](iced-linux-x11-wgpu-1/shell.png)", rendered)
        self.assertIn("[palette](iced-linux-x11-wgpu-1/palette.png)", rendered)
        self.assertIn("dirty", rendered)
        self.assertIn("`" + "c" * 64 + "`", rendered)
        self.assertIn("not golden-image assertions", rendered)

    def test_stale_run_or_commit_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "different run"):
            parity.validate_measurement(self.document(), "run-8", "abc123")
        with self.assertRaisesRegex(ValueError, "different commit"):
            parity.validate_measurement(self.document(), "run-7", "def456")

    def test_declared_unavailable_palette_needs_no_false_provenance(self):
        document = self.document()
        document["palette"] = {
            "available": False,
            "reason": "product capture API excludes child panel",
        }
        parity.validate_measurement(document, "run-7", "abc123")


class RunnerTests(unittest.TestCase):
    def test_explicit_binary_override_is_used_for_provenance(self):
        with mock.patch.dict(
            parity.os.environ,
            {"ROOST_ICED_BIN": "/shed/rt/debug/roost-iced"},
            clear=True,
        ):
            self.assertEqual(
                parity._executable_path(Path("/repo"), "iced"),
                Path("/shed/rt/debug/roost-iced"),
            )

    def test_run_id_is_one_safe_path_component(self):
        self.assertEqual(parity._validate_run_id("commit-run_1.2"), "commit-run_1.2")
        for invalid in ("", ".", "..", "../escape", "/tmp/escape", "two/parts"):
            with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                parity._validate_run_id(invalid)

    @mock.patch.object(parity.shutil, "which", return_value="/opt/bin/uv")
    def test_uses_uv_environment_when_available(self, _which):
        self.assertEqual(
            parity._pytest_command(),
            ["uv", "run", "--group", "test", "pytest"],
        )

    @mock.patch.object(parity.shutil, "which", return_value=None)
    def test_falls_back_to_current_python(self, _which):
        self.assertEqual(
            parity._pytest_command(),
            [sys.executable, "-m", "pytest"],
        )


if __name__ == "__main__":
    unittest.main()
