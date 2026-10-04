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


def logical_lines(text: str) -> list[str]:
    """Make's view of a file: backslash continuations joined, `#` comments dropped."""
    joined: list[str] = []
    pending = ""
    for line in text.splitlines():
        if line.endswith("\\"):
            pending += line[:-1] + " "
            continue
        joined.append(pending + line)
        pending = ""
    if pending:
        joined.append(pending)
    return [line.split("#", 1)[0].rstrip() if "#" in line else line for line in joined]


ASSIGNMENT = re.compile(r"(?:(override|export)\s+)?([A-Z][A-Z0-9_]*_TESTS)\s*(\S*?)=\s*(.*)$")
DEFINE = re.compile(r"(?:override\s+)?define\s+[A-Z][A-Z0-9_]*_TESTS\b")
REFERENCE = re.compile(r"\$[({][A-Z][A-Z0-9_]*_TESTS[)}]")


def parse_makefile_lists(text: str) -> dict[str, list[str]]:
    """Every `*_TESTS` variable's modules. `=`, `:=`, `::=` replace and `+=`
    appends; a form this parser cannot honour (`?=`, `!=`, `define`, a prefix,
    a reference to another list) raises rather than guessing."""
    lists: dict[str, list[str]] = {}
    for line in logical_lines(text):
        if DEFINE.match(line):
            raise AssertionError(f"unsupported Makefile `define` of a list: {line!r}")
        match = ASSIGNMENT.match(line)
        if not match:
            continue
        prefix, name, operator, value = match.groups()
        if prefix or operator not in ("", ":", "::", "+"):
            raise AssertionError(f"unsupported assignment to {name}: {line!r}")
        if REFERENCE.search(value):
            raise AssertionError(f"{name} references another list, which this parser does not expand")
        modules = MODULE.findall(value)
        if operator == "+":
            lists.setdefault(name, []).extend(modules)
        else:
            lists[name] = modules
    return lists


def parse_recipe_modules(text: str, target: str) -> list[str]:
    lines = logical_lines(text)
    start = next((i for i, line in enumerate(lines) if line.startswith(f"{target}:")), None)
    if start is None:
        raise AssertionError(f"Makefile has no target {target!r}")
    modules: list[str] = []
    for line in lines[start + 1 :]:
        if not line.startswith("\t"):
            break
        modules += MODULE.findall(line)
    return modules


def makefile_lists() -> dict[str, list[str]]:
    return parse_makefile_lists(MAKEFILE.read_text())


def ci_step_modules(name: str) -> list[str]:
    return parse_ci_step_modules(CI.read_text(), name)


def parse_ci_step_modules(text: str, name: str) -> list[str]:
    lines = text.splitlines()
    start = next((i for i, line in enumerate(lines) if line.strip() == f"- name: {name}"), None)
    if start is None:
        raise AssertionError(f"ci.yml has no step named {name!r}")
    modules: list[str] = []
    for line in lines[start + 1 :]:
        if line.startswith("      - name:"):
            break
        if line.lstrip().startswith("#"):
            continue
        modules += MODULE.findall(re.split(r"\s#", line, maxsplit=1)[0])
    return modules


def makefile_recipe_modules(target: str) -> list[str]:
    return parse_recipe_modules(MAKEFILE.read_text(), target)


def on_disk() -> set[str]:
    return {f"tools/roosttest/{path.name}" for path in ROOSTTEST.glob("test_*.py")}


LANE = "tools/roosttest/test_a.py"
OTHER = "tools/roosttest/test_b.py"


class MakefileParserTests(unittest.TestCase):
    def test_a_commented_out_value_is_empty(self) -> None:
        text = f"LOCAL_BACKEND_E2E_TESTS := # {LANE}\n"
        self.assertEqual(parse_makefile_lists(text), {"LOCAL_BACKEND_E2E_TESTS": []})

    def test_a_trailing_comment_is_dropped(self) -> None:
        text = f"X_TESTS := {LANE} # {OTHER}\n"
        self.assertEqual(parse_makefile_lists(text)["X_TESTS"], [LANE])

    def test_an_append_extends_the_variable(self) -> None:
        text = f"ICED_E2E_TESTS := {LANE}\nICED_E2E_TESTS += {OTHER}\n"
        self.assertEqual(parse_makefile_lists(text)["ICED_E2E_TESTS"], [LANE, OTHER])

    def test_an_append_with_no_prior_definition_starts_the_list(self) -> None:
        self.assertEqual(parse_makefile_lists(f"X_TESTS += {LANE}\n")["X_TESTS"], [LANE])

    def test_a_plain_reassignment_replaces(self) -> None:
        text = f"X_TESTS = {LANE}\nX_TESTS := {OTHER}\n"
        self.assertEqual(parse_makefile_lists(text)["X_TESTS"], [OTHER])

    def test_a_continuation_line_joins_the_value(self) -> None:
        text = f"LOCAL_BACKEND_E2E_TESTS := {LANE} \\\n    {OTHER}\n"
        self.assertEqual(parse_makefile_lists(text)["LOCAL_BACKEND_E2E_TESTS"], [LANE, OTHER])

    def test_a_continuation_after_a_comment_is_still_a_comment(self) -> None:
        text = f"X_TESTS := {LANE} # note \\\n {OTHER}\nY_TESTS := {OTHER}\n"
        self.assertEqual(parse_makefile_lists(text), {"X_TESTS": [LANE], "Y_TESTS": [OTHER]})

    def test_unsupported_forms_fail(self) -> None:
        for text in (
            f"X_TESTS ?= {LANE}\n",
            f"X_TESTS != echo {LANE}\n",
            f"define X_TESTS\n{LANE}\nendef\n",
            f"override X_TESTS := {LANE}\n",
            f"export X_TESTS := {LANE}\n",
            f"X_TESTS := {LANE}\nY_TESTS := $(X_TESTS) {OTHER}\n",
        ):
            with self.subTest(text=text), self.assertRaises(AssertionError):
                parse_makefile_lists(text)

    def test_recipe_comments_do_not_count(self) -> None:
        text = f"t:\n\tpytest {LANE}\n\t# pytest {OTHER}\n\tpytest x # {OTHER}\nnext:\n"
        self.assertEqual(parse_recipe_modules(text, "t"), [LANE])

    def test_recipe_continuations_join(self) -> None:
        text = f"t:\n\tpytest {LANE} \\\n\t  {OTHER}\n"
        self.assertEqual(parse_recipe_modules(text, "t"), [LANE, OTHER])

    def test_ci_step_comments_do_not_count(self) -> None:
        text = (
            "      - name: Lane\n"
            "        run: >\n"
            f"          pytest {LANE}\n"
            f"          # {OTHER}\n"
            f"          {LANE.replace('test_a', 'test_c')} # {OTHER}\n"
            "      - name: Next\n"
        )
        self.assertEqual(parse_ci_step_modules(text, "Lane"), [LANE, "tools/roosttest/test_c.py"])


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
