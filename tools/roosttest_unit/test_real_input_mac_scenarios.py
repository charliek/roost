"""`test_real_input_mac.SCENARIOS` names exactly the module's tests (plan 074 §D7, §D8).

The CI promotion rule counts the real-input suite as qualified only when the
JUnit report lists every expected scenario as passed, so the expected list
must not drift from the tests themselves: a test added without an entry would
never be required, and an entry without a test would never pass. Read with
`ast`, since the module imports pytest and this suite's interpreter may not.
"""

from __future__ import annotations

import ast
import unittest
from pathlib import Path

MODULE = Path(__file__).resolve().parents[1] / "roosttest" / "test_real_input_mac.py"


def scenarios_and_tests() -> tuple[list[str], list[str]]:
    tree = ast.parse(MODULE.read_text())
    listed: list[str] = []
    tests: list[str] = []
    for node in tree.body:
        if isinstance(node, ast.Assign) and any(
            isinstance(target, ast.Name) and target.id == "SCENARIOS" for target in node.targets
        ):
            listed = list(ast.literal_eval(node.value))
        elif isinstance(node, ast.FunctionDef) and node.name.startswith("test_"):
            if node.decorator_list:
                raise AssertionError(
                    f"{node.name} is decorated: a parametrized test's ids are not its name"
                )
            tests.append(node.name)
    return listed, tests


class ScenarioListTests(unittest.TestCase):
    def test_the_list_is_the_modules_tests_in_order(self) -> None:
        listed, tests = scenarios_and_tests()
        self.assertTrue(listed, "SCENARIOS is missing or empty")
        self.assertEqual(listed, tests)

    def test_preflight_runs_first(self) -> None:
        listed, _ = scenarios_and_tests()
        self.assertEqual(listed[0], "test_preflight")


if __name__ == "__main__":
    unittest.main()
