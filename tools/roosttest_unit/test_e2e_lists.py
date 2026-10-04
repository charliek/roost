"""The Makefile's E2E lists and `ci.yml`'s hand-mirrored lane lists agree (plan 075 §D4.3).

`ci.yml` cannot reference a Makefile variable, so each curated lane restates
its module list. A module added to one place and not the other runs on some
gates and silently not on others. Every list is located by name; one that is
missing or parses empty is a failure, never an empty set.
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
MAKEFILE = REPO / "Makefile"
CI = REPO / ".github" / "workflows" / "ci.yml"
ROOSTTEST = REPO / "tools" / "roosttest"

MODULE = re.compile(r"tools/roosttest/test_\w+\.py")

# macOS runs ICED_E2E_TESTS + ICED_CLIPBOARD_TESTS minus these. Neither has a
# self-skip or marker that confines it to Linux: both docstrings say they run
# on every target, and `git log -S` shows they entered the Linux lists first
# and the macOS list never took them. The omission is historical, not a
# recorded reason: they are exceptions here so the gap is explicit, and
# closing it is a separate decision.
MACOS_EXCEPTIONS = {
    # No Linux-only skip or marker; its Linux/macOS login-shell split asserts on sys.platform.
    "tools/roosttest/test_shell_integration.py",
    # No skip or marker; the module docstring says it is real on Linux and macOS.
    "tools/roosttest/test_newtab_cwd.py",
    # `sys.platform != "linux"` module-level skip (/proc sweep, sun_path sizing).
    "tools/roosttest/test_boot_failure.py",
    # `sys.platform != "linux"` module-level skip (/proc sweep of the UI's processes).
    "tools/roosttest/test_quit_with_foreground_job.py",
}

# Makefile variables that name a dedicated lane, each run by its own invocation.
DEDICATED_VARIABLES = {
    "ICED_EXIT_E2E_TESTS",
    "ICED_MENU_QUIT_E2E_TESTS",
    "ICED_RELEASE_E2E_TESTS",
    "SESSION_E2E_TESTS",
    "HOST_CLIENT_E2E_TESTS",
    "SSH_HOST_E2E_TESTS",
    "MISSING_DAEMON_E2E_TESTS",
    "LOCAL_SPAWN_E2E_TESTS",
    "LOCALHOST_E2E_TESTS",
    "LOCAL_BACKEND_E2E_TESTS",
    "OLD_SESSION_E2E_TESTS",
    "BOOTSTRAP_E2E_TESTS",
}

# Swift-only modules: `e2e-mac` collects the whole directory (Makefile
# `e2e-mac`), so they need no list entry; the iced lanes do not run them.
SWIFT_SWEEP_ONLY = {
    "test_launcher",
    "test_ordering",
    "test_sidebar_agents",
    "test_sidebar_collapse_persistence",
    "test_sidebar_layout",
    "test_terminal",
    "test_test_ops",
    "test_word_selection",
}

X11_STEP = "Run Iced functional E2E (Linux X11)"
WAYLAND_STEP = "Run Iced functional E2E (Linux Wayland)"
MACOS_STEP = "Run Iced functional E2E (macOS)"
REAL_INPUT_TARGET = "e2e-iced-real-input-mac"


def makefile_lists() -> dict[str, list[str]]:
    lists: dict[str, list[str]] = {}
    for line in MAKEFILE.read_text().splitlines():
        match = re.match(r"([A-Z][A-Z0-9_]*_TESTS)\s*:?=\s*(.*)$", line)
        if match:
            lists[match.group(1)] = MODULE.findall(match.group(2))
    return lists


def ci_step_modules(name: str) -> list[str]:
    lines = CI.read_text().splitlines()
    start = next((i for i, line in enumerate(lines) if line.strip() == f"- name: {name}"), None)
    if start is None:
        raise AssertionError(f"ci.yml has no step named {name!r}")
    modules: list[str] = []
    for line in lines[start + 1 :]:
        if line.startswith("      - name:"):
            break
        if line.lstrip().startswith("#"):
            continue
        modules += MODULE.findall(line)
    return modules


def makefile_recipe_modules(target: str) -> list[str]:
    lines = MAKEFILE.read_text().splitlines()
    start = next((i for i, line in enumerate(lines) if line.startswith(f"{target}:")), None)
    if start is None:
        raise AssertionError(f"Makefile has no target {target!r}")
    modules: list[str] = []
    for line in lines[start + 1 :]:
        if not line.startswith("\t"):
            break
        modules += MODULE.findall(line)
    return modules


def on_disk() -> set[str]:
    return {f"tools/roosttest/{path.name}" for path in ROOSTTEST.glob("test_*.py")}


class ListTests(unittest.TestCase):
    def nonempty_unique(self, name: str, modules: list[str]) -> set[str]:
        self.assertTrue(modules, f"{name} parsed empty")
        duplicates = sorted({m for m in modules if modules.count(m) > 1})
        self.assertEqual(duplicates, [], f"{name} lists a module twice")
        return set(modules)

    def make(self, name: str) -> set[str]:
        lists = makefile_lists()
        self.assertIn(name, lists, f"Makefile has no {name}")
        return self.nonempty_unique(name, lists[name])

    def assertLane(self, name: str, actual: set[str], expected: set[str]) -> None:
        self.assertEqual(sorted(actual - expected), [], f"{name} has modules the Makefile does not")
        self.assertEqual(sorted(expected - actual), [], f"{name} lacks modules the Makefile has")

    def test_the_x11_lane_is_iced_plus_clipboard(self) -> None:
        actual = self.nonempty_unique(X11_STEP, ci_step_modules(X11_STEP))
        self.assertLane(X11_STEP, actual, self.make("ICED_E2E_TESTS") | self.make("ICED_CLIPBOARD_TESTS"))

    def test_the_wayland_lane_is_iced_without_clipboard(self) -> None:
        actual = self.nonempty_unique(WAYLAND_STEP, ci_step_modules(WAYLAND_STEP))
        self.assertLane(WAYLAND_STEP, actual, self.make("ICED_E2E_TESTS"))

    def test_the_macos_lane_is_iced_plus_clipboard_minus_the_exceptions(self) -> None:
        actual = self.nonempty_unique(MACOS_STEP, ci_step_modules(MACOS_STEP))
        expected = self.make("ICED_E2E_TESTS") | self.make("ICED_CLIPBOARD_TESTS")
        self.assertLane(MACOS_STEP, actual, expected - MACOS_EXCEPTIONS)

    def test_the_macos_exceptions_are_real_modules_in_the_makefile(self) -> None:
        iced = self.make("ICED_E2E_TESTS")
        for module in MACOS_EXCEPTIONS:
            self.assertIn(module, iced, f"{module} is no longer an iced module: drop its exception")

    def test_every_makefile_list_is_classified(self) -> None:
        names = set(makefile_lists())
        known = {"ICED_E2E_TESTS", "ICED_CLIPBOARD_TESTS"} | DEDICATED_VARIABLES
        self.assertEqual(sorted(names - known), [], "a new *_TESTS variable: classify it here")
        self.assertEqual(sorted(known - names), [], "a classified variable is gone from the Makefile")
        for name in sorted(names):
            self.nonempty_unique(name, makefile_lists()[name])

    def test_every_module_is_classified(self) -> None:
        classified: set[str] = set()
        for modules in makefile_lists().values():
            classified |= set(modules)
        real_input = makefile_recipe_modules(REAL_INPUT_TARGET)
        self.assertEqual(real_input, ["tools/roosttest/test_real_input_mac.py"])
        classified |= set(real_input)
        classified |= {f"tools/roosttest/{name}.py" for name in SWIFT_SWEEP_ONLY}
        disk = on_disk()
        self.assertEqual(sorted(disk - classified), [], "modules in no lane and no named class")
        self.assertEqual(sorted(classified - disk), [], "a list names a module that does not exist")


if __name__ == "__main__":
    unittest.main()
