#!/usr/bin/env python3
"""Capture and summarize the same hermetic visual fixture across Roost UIs.

The actual UI drive lives in ``tools/roosttest/parity_capture.py`` so it can
reuse the functional harness's launch, state, and socket isolation. This file
contains the renderer-neutral pixel measurements plus an orchestrator that
runs requested targets sequentially and aggregates their provenance.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import tempfile
import uuid
from pathlib import Path
from typing import Iterable

import pngtool

SCHEMA_VERSION = 2
SCENARIO = "workspace-shell-v1"
SCALE = 1
TERMINAL_BACKGROUND = (0x1E, 0x1E, 0x1E)
PALETTE_SELECTION = (0x48, 0x48, 0x4E)
PALETTE_PRIMARY_TEXT = {(0xF2, 0xF2, 0xF2), (0xFF, 0xFF, 0xFF)}
LIFECYCLE_COLORS = {
    "working": (0x5F, 0xA3, 0xF0),
    "waiting": (0xF0, 0xA0, 0x40),
    "finished": (0x7A, 0x7A, 0x7A),
    "failed": (0xE0, 0x52, 0x52),
}
PALETTE_VARIANTS = ("commands", "query", "agents", "notifications", "provider")

Image = tuple[int, int, int, bytes]


def pixel(image: Image, x: int, y: int) -> tuple[int, int, int]:
    width, height, bpp, data = image
    if not (0 <= x < width and 0 <= y < height):
        raise ValueError(f"pixel ({x},{y}) outside {width}x{height}")
    offset = (y * width + x) * bpp
    return data[offset], data[offset + 1], data[offset + 2]


def count_color(
    image: Image,
    color: tuple[int, int, int],
    bounds: tuple[int, int, int, int] | None = None,
) -> int:
    width, height, _bpp, _data = image
    left, top, right, bottom = bounds or (0, 0, width, height)
    left, top = max(0, left), max(0, top)
    right, bottom = min(width, right), min(height, bottom)
    return sum(
        pixel(image, x, y) == color
        for y in range(top, bottom)
        for x in range(left, right)
    )


def first_vertical_run(
    image: Image,
    x: int,
    color: tuple[int, int, int],
    minimum: int = 8,
) -> int | None:
    _width, height, _bpp, _data = image
    start: int | None = None
    for y in range(height):
        if pixel(image, x, y) == color:
            start = y if start is None else start
            if y - start + 1 >= minimum:
                return start
        else:
            start = None
    return None


def first_horizontal_run(
    image: Image,
    y: int,
    color: tuple[int, int, int],
    minimum: int = 8,
) -> int | None:
    width, _height, _bpp, _data = image
    start: int | None = None
    for x in range(width):
        if pixel(image, x, y) == color:
            start = x if start is None else start
            if x - start + 1 >= minimum:
                return start
        else:
            start = None
    return None


def _components(points: set[tuple[int, int]]) -> list[dict[str, int]]:
    seen: set[tuple[int, int]] = set()
    components = []
    for origin in points:
        if origin in seen:
            continue
        seen.add(origin)
        stack = [origin]
        group = []
        while stack:
            x, y = stack.pop()
            group.append((x, y))
            for neighbor in ((x - 1, y), (x + 1, y), (x, y - 1), (x, y + 1)):
                if neighbor in points and neighbor not in seen:
                    seen.add(neighbor)
                    stack.append(neighbor)
        xs, ys = zip(*group)
        components.append(
            {
                "left": min(xs),
                "top": min(ys),
                "right": max(xs),
                "bottom": max(ys),
                "pixels": len(group),
            }
        )
    return sorted(components, key=lambda item: (item["top"], item["left"]))


def color_components(
    image: Image,
    color: tuple[int, int, int],
    bounds: tuple[int, int, int, int],
    minimum_side: int = 5,
) -> list[dict[str, int]]:
    left, top, right, bottom = bounds
    points = {
        (x, y)
        for y in range(top, bottom)
        for x in range(left, right)
        if pixel(image, x, y) == color
    }
    return [
        component
        for component in _components(points)
        if component["right"] - component["left"] + 1 >= minimum_side
        and component["bottom"] - component["top"] + 1 >= minimum_side
    ]


def unique_rollup_stripe_bounds(
    image: Image,
    color: tuple[int, int, int],
    sidebar_width: int,
    body_top: int,
    body_bottom: int,
) -> tuple[int, int, int, int] | None:
    """Find one plausible project-rollup stripe in the sidebar body.

    Roost's rollup stripe is a narrow, row-height component at the leading
    edge. Same-color terminal glyphs, tab dots, and agent dots are deliberately
    outside this search region or fail the stripe geometry. Multiple plausible
    components are ambiguous and therefore a hard harness error.
    """
    width, height, _bpp, _data = image
    right = max(0, min(sidebar_width, width, 8))
    top = max(0, min(body_top, height))
    bottom = max(top, min(body_bottom, height))
    points = {
        (x, y)
        for y in range(top, bottom)
        for x in range(right)
        if pixel(image, x, y) == color
    }
    plausible = []
    for component in _components(points):
        component_width = component["right"] - component["left"] + 1
        component_height = component["bottom"] - component["top"] + 1
        if (
            2 <= component_width <= 5
            and 18 <= component_height <= 40
            and component["left"] <= 1
        ):
            plausible.append(component)
    if len(plausible) > 1:
        raise ValueError(f"multiple plausible rollup stripes: {plausible!r}")
    if not plausible:
        return None
    component = plausible[0]
    return (
        component["left"],
        component["top"],
        component["right"],
        component["bottom"],
    )


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def central_region_digest(path: Path) -> str:
    width, height, bpp, data = pngtool.load(str(path))
    left, right = width // 4, width * 3 // 4
    top, bottom = height // 5, height * 4 // 5
    digest = hashlib.sha256()
    for y in range(top, bottom):
        start = (y * width + left) * bpp
        end = (y * width + right) * bpp
        digest.update(data[start:end])
    return digest.hexdigest()


def measure_shell(path: Path, sidebar_width: int) -> dict:
    image = pngtool.load(str(path))
    width, height, _bpp, _data = image
    sidebar_width = max(1, min(sidebar_width, width - 1))
    terminal_probe_x = min(width - 1, sidebar_width + 300)
    terminal_probe_y = height // 2
    lifecycle = {
        name: color_components(
            image,
            color,
            (0, 0, sidebar_width, height),
        )
        for name, color in LIFECYCLE_COLORS.items()
    }
    return {
        "png": path.name,
        "sha256": sha256(path),
        "width": width,
        "height": height,
        "sidebar_sample": pixel(image, 5, height // 2),
        "terminal_sample": pixel(image, terminal_probe_x, terminal_probe_y),
        "terminal_top": first_vertical_run(
            image, terminal_probe_x, TERMINAL_BACKGROUND
        ),
        "terminal_left": first_horizontal_run(
            image, terminal_probe_y, TERMINAL_BACKGROUND
        ),
        "lifecycle_components": lifecycle,
    }


def measure_agent_palette(path: Path, sidebar_width: int) -> dict:
    """Measure the selected failed row's semantic column geometry.

    This deliberately checks relationships rather than whole-image equality:
    the status must trail the name, while muted metrics/time remain visible to
    its right. GTK and Iced use the same selection/lifecycle colors but retain
    their normal font rasterization differences.
    """
    image = pngtool.load(str(path))
    width, height, _bpp, _data = image
    selected = color_components(
        image,
        PALETTE_SELECTION,
        (sidebar_width, 0, width, height),
        minimum_side=12,
    )
    if not selected:
        raise ValueError("agent palette has no selected-row background")
    bounds = max(selected, key=lambda item: item["pixels"])
    left, top = bounds["left"], bounds["top"]
    right, bottom = bounds["right"], bounds["bottom"]
    primary_x = [
        x
        for y in range(top, bottom + 1)
        for x in range(left + 24, right + 1)
        if pixel(image, x, y) in PALETTE_PRIMARY_TEXT
    ]
    status_x = [
        x
        for y in range(top, bottom + 1)
        for x in range(left + 32, right + 1)
        if pixel(image, x, y) == LIFECYCLE_COLORS["failed"]
    ]
    if not primary_x or not status_x:
        raise ValueError("agent palette selected row is missing name or failed status text")
    name_right = max(primary_x)
    status_left, status_right = min(status_x), max(status_x)
    trailing_ink_pixels = sum(
        pixel(image, x, y) != PALETTE_SELECTION
        for y in range(top + 6, bottom - 5)
        for x in range(max(status_right + 2, right - 80), right - 3)
    )
    return {
        "selected_bounds": bounds,
        "name_right": name_right,
        "status_left": status_left,
        "status_right": status_right,
        "status_ink_span": status_right - status_left + 1,
        "name_status_gap": status_left - name_right - 1,
        # The rightmost 80px are reserved for the compact metrics/time column.
        # Count rasterized ink rather than one exact foreground because GTK
        # alpha-blends its text color into the selected-row background.
        "trailing_ink_pixels": trailing_ink_pixels,
    }
def environment_metadata(target: str, run_id: str, commit: str) -> dict[str, object]:
    system = platform.system().lower()
    backend_override = None
    if system == "linux" and target == "gtk":
        backend_override = os.environ.get("GDK_BACKEND")
    elif system == "linux" and target == "iced":
        backend_override = os.environ.get("WINIT_UNIX_BACKEND")
    if backend_override:
        display = backend_override
    elif system == "linux" and os.environ.get("WAYLAND_DISPLAY"):
        display = "wayland"
    elif system == "linux" and os.environ.get("DISPLAY"):
        display = "x11"
    else:
        display = "native"
    if target == "iced":
        renderer = os.environ.get("ICED_BACKEND", "default")
    elif target == "gtk":
        renderer = os.environ.get("GSK_RENDERER", "gtk-default")
    else:
        renderer = "appkit"
    executable = Path(os.environ["ROOST_PARITY_EXECUTABLE"])
    return {
        "schema_version": SCHEMA_VERSION,
        "scenario": SCENARIO,
        "target": target,
        "os": system,
        "display_backend": display,
        "renderer": renderer,
        "scale": SCALE,
        "commit": commit,
        "run_id": run_id,
        "backend_environment": {
            name: os.environ[name]
            for name in ("GDK_BACKEND", "WINIT_UNIX_BACKEND", "WAYLAND_DISPLAY", "DISPLAY")
            if name in os.environ
        },
        "dynamic_regions": ["agent elapsed time"],
        "source_dirty": os.environ.get("ROOST_PARITY_SOURCE_DIRTY") == "1",
        "executable": {
            "path": str(executable),
            "sha256": sha256(executable),
        },
    }


def environment_key(metadata: dict[str, object]) -> str:
    raw = "-".join(
        str(metadata[key])
        for key in ("target", "os", "display_backend", "renderer", "scale")
    )
    return re.sub(r"[^a-zA-Z0-9_.-]+", "-", raw).strip("-")


def atomic_json(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(
        mode="w", encoding="utf-8", dir=path.parent, delete=False
    ) as handle:
        json.dump(value, handle, indent=2, sort_keys=True)
        handle.write("\n")
        handle.flush()
        os.fsync(handle.fileno())
        temporary = Path(handle.name)
    os.replace(temporary, path)


def validate_measurement(document: dict, run_id: str, commit: str) -> None:
    metadata = document.get("metadata", {})
    if metadata.get("schema_version") != SCHEMA_VERSION:
        raise ValueError("measurement schema version mismatch")
    if metadata.get("scenario") != SCENARIO:
        raise ValueError("measurement scenario mismatch")
    if metadata.get("run_id") != run_id:
        raise ValueError("measurement belongs to a different run")
    if metadata.get("commit") != commit:
        raise ValueError("measurement belongs to a different commit")
    if not document.get("shell", {}).get("sha256"):
        raise ValueError("measurement is missing shell provenance")
    if not metadata.get("executable", {}).get("sha256"):
        raise ValueError("measurement is missing executable provenance")
    if not isinstance(metadata.get("source_dirty"), bool):
        raise ValueError("measurement is missing source status")
    palette = document.get("palette", {})
    if palette.get("available") is not False and not palette.get("sha256"):
        raise ValueError("measurement is missing palette provenance")
    if palette.get("available") is not False:
        variants = palette.get("variants", {})
        for name in PALETTE_VARIANTS:
            variant = variants.get(name, {})
            if not variant.get("png") or not variant.get("sha256"):
                raise ValueError(f"measurement is missing {name} palette provenance")


def format_manifest(documents: Iterable[dict], run_id: str, commit: str) -> str:
    rows = []
    for document in sorted(
        documents,
        key=lambda item: environment_key(item["metadata"]),
    ):
        validate_measurement(document, run_id, commit)
        metadata = document["metadata"]
        shell = document["shell"]
        key = environment_key(metadata)
        palette = document["palette"]
        executable = metadata["executable"]
        if palette.get("available") is not False:
            palette_link = " ".join(
                f"[{name}]({key}/{palette['variants'][name]['png']})"
                for name in PALETTE_VARIANTS
            )
        else:
            palette_link = f"unavailable: {palette.get('reason', 'unspecified')}"
        rows.append(
            "| {target} | {os} | {display} | {renderer} | {size} | {sidebar} | "
            "{top} | [shell]({shell_link}) | {palette_link} | {source} | "
            "`{executable}` | `{digest}` |".format(
                target=metadata["target"],
                os=metadata["os"],
                display=metadata["display_backend"],
                renderer=metadata["renderer"],
                size=f"{shell['width']}×{shell['height']}",
                sidebar=tuple(shell["sidebar_sample"]),
                top=shell["terminal_top"],
                shell_link=f"{key}/{shell['png']}",
                palette_link=palette_link,
                source="dirty" if metadata["source_dirty"] else "clean",
                executable=executable["sha256"],
                digest=shell["sha256"],
            )
        )
    header = (
        f"# Roost visual parity capture\n\n"
        f"- Run: `{run_id}`\n"
        f"- Commit: `{commit}`\n"
        f"- Schema: `{SCHEMA_VERSION}` / scenario `{SCENARIO}`\n\n"
        "Hashes record provenance only; they are not golden-image assertions.\n\n"
        "Agent elapsed times are dynamic and excluded from visual comparison.\n\n"
        "| Target | OS | Display | Renderer | Shell size | Sidebar sample | "
        "Terminal top | Shell | Palettes | Source | Executable SHA-256 | "
        "Shell SHA-256 |\n"
        "|---|---|---|---|---:|---|---:|---|---|---|---|---|\n"
    )
    return header + "\n".join(rows) + "\n"


def _git_commit(repo: Path) -> str:
    return subprocess.check_output(
        ["git", "-c", f"safe.directory={repo}", "rev-parse", "HEAD"],
        cwd=repo,
        text=True,
    ).strip()


def _source_dirty(repo: Path) -> bool:
    output = subprocess.check_output(
        [
            "git",
            "-c",
            f"safe.directory={repo}",
            "status",
            "--porcelain",
            "--untracked-files=normal",
        ],
        cwd=repo,
        text=True,
    )
    return bool(output.strip())


def _executable_path(repo: Path, target: str) -> Path:
    if target == "mac":
        return repo / "mac/build/Roost.app/Contents/MacOS/Roost"
    environment_name = "ROOST_GTK_BIN" if target == "gtk" else "ROOST_ICED_BIN"
    default_name = "roost" if target == "gtk" else "roost-iced"
    configured = os.environ.get(environment_name)
    if configured:
        path = Path(configured)
        return path if path.is_absolute() else repo / path
    return repo / "target/debug" / default_name


def _build_targets(repo: Path, targets: Iterable[str]) -> None:
    commands = {
        "mac": (["./scripts/bundle.sh", "debug"], repo / "mac"),
        "gtk": (["cargo", "build", "-p", "roost-linux"], repo),
        "iced": (["cargo", "build", "-p", "roost-iced"], repo),
    }
    for target in targets:
        command, cwd = commands[target]
        subprocess.run(command, cwd=cwd, check=True)


def _default_targets() -> list[str]:
    return ["mac", "gtk", "iced"] if platform.system() == "Darwin" else ["gtk", "iced"]


def _pytest_command() -> list[str]:
    if shutil.which("uv"):
        return ["uv", "run", "--group", "test", "pytest"]
    return [sys.executable, "-m", "pytest"]


def _validate_run_id(run_id: str) -> str:
    if run_id in {"", ".", ".."} or not re.fullmatch(
        r"[A-Za-z0-9][A-Za-z0-9_.-]*", run_id
    ):
        raise ValueError("run ID must be one safe filename component")
    return run_id


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", type=Path, default=Path("target/visual-parity"))
    parser.add_argument("--targets", nargs="+", choices=("mac", "gtk", "iced"))
    parser.add_argument("--run-id")
    parser.add_argument(
        "--no-build",
        action="store_true",
        help="use already-built binaries (required for shed-local artifact isolation)",
    )
    args = parser.parse_args(argv)

    repo = Path(__file__).resolve().parents[2]
    commit = _git_commit(repo)
    try:
        run_id = _validate_run_id(
            args.run_id or f"{commit[:7]}-{uuid.uuid4().hex[:10]}"
        )
    except ValueError as error:
        parser.error(str(error))
    targets = args.targets or _default_targets()
    if not args.no_build:
        _build_targets(repo, targets)
    executables = {target: _executable_path(repo, target) for target in targets}
    missing = [str(path) for path in executables.values() if not path.is_file()]
    if missing:
        parser.error(f"capture executable does not exist: {', '.join(missing)}")
    source_dirty = _source_dirty(repo)
    output = args.out.resolve()
    run_root = output / run_id
    run_root.mkdir(parents=True, exist_ok=False)

    failures = []
    for target in targets:
        env = os.environ.copy()
        env.update(
            {
                "ROOST_TEST_MODE": "1",
                "ROOST_TEST_FRESH": "1",
                "ROOST_PARITY_OUTPUT_BASE": str(output),
                "ROOST_PARITY_RUN_ID": run_id,
                "ROOST_PARITY_COMMIT": commit,
                "ROOST_PARITY_SOURCE_DIRTY": "1" if source_dirty else "0",
                "ROOST_PARITY_EXECUTABLE": str(executables[target]),
            }
        )
        command = [
            *_pytest_command(),
            "tools/roosttest/parity_capture.py",
            "--roost-target",
            target,
            "--roost-fresh",
            "-q",
        ]
        result = subprocess.run(command, cwd=repo, env=env, check=False)
        if result.returncode:
            failures.append(f"{target}: pytest exited {result.returncode}")

    documents = []
    for path in sorted(run_root.glob("*/measurements.json")):
        try:
            document = json.loads(path.read_text())
            validate_measurement(document, run_id, commit)
            documents.append(document)
        except (OSError, ValueError, json.JSONDecodeError) as error:
            failures.append(f"{path}: {error}")
    captured_targets = {document["metadata"]["target"] for document in documents}
    for missing in sorted(set(targets) - captured_targets):
        failures.append(f"{missing}: no current-run measurements.json")

    manifest = format_manifest(documents, run_id, commit)
    (run_root / "manifest.md").write_text(manifest)
    print(run_root)
    if failures:
        print("visual parity capture failed:", file=sys.stderr)
        for failure in failures:
            print(f"- {failure}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
