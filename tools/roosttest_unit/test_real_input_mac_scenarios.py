"""`test_real_input_mac.SCENARIOS` names exactly the module's tests (plan 074 §D7, §D8).

The CI promotion rule counts the real-input suite as qualified only when the
JUnit report lists every expected scenario as passed, so the expected list
must not drift from the tests themselves: a test added without an entry would
never be required, and an entry without a test would never pass. Read with
`ast` (`junit_guard.scenarios_and_tests`, shared with the CI guard), since the
module imports pytest and this suite's interpreter may not.
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

ROOSTTEST_DIR = Path(__file__).resolve().parents[1] / "roosttest"
MODULE = ROOSTTEST_DIR / "test_real_input_mac.py"
sys.path.insert(0, str(ROOSTTEST_DIR))

import junit_guard  # noqa: E402


def scenarios_and_tests() -> tuple[list[str], list[str]]:
    return junit_guard.scenarios_and_tests(MODULE)


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
