"""`junit_guard` passes only a report that proves every scenario ran clean (plan 075 §D4.1)."""

from __future__ import annotations

import contextlib
import io
import sys
import tempfile
import unittest
from pathlib import Path

ROOSTTEST_DIR = Path(__file__).resolve().parents[1] / "roosttest"
sys.path.insert(0, str(ROOSTTEST_DIR))

import junit_guard  # noqa: E402

EXPECTED = ["test_a", "test_b", "test_c"]


def case(name: str, child: str = "") -> str:
    return f'<testcase classname="m" name="{name}" time="0.1">{child}</testcase>'


def suite(*cases: str) -> str:
    return f'<testsuite name="pytest" tests="{len(cases)}">{"".join(cases)}</testsuite>'


PASS = suite(*(case(name) for name in EXPECTED))


class ProblemsTests(unittest.TestCase):
    def check(self, xml: str, *needles: str) -> None:
        found = junit_guard.problems(xml, EXPECTED)
        self.assertTrue(found, "expected the guard to fail")
        text = "\n".join(found)
        for needle in needles:
            self.assertIn(needle, text)

    def test_a_clean_report_passes(self) -> None:
        self.assertEqual(junit_guard.problems(PASS, EXPECTED), [])

    def test_a_testsuites_root_passes(self) -> None:
        self.assertEqual(junit_guard.problems(f"<testsuites>{PASS}</testsuites>", EXPECTED), [])

    def test_nested_suites_are_not_double_counted(self) -> None:
        nested = f"<testsuites><testsuite>{PASS}</testsuite></testsuites>"
        self.assertEqual(junit_guard.problems(nested, EXPECTED), [])

    def test_a_missing_scenario_fails(self) -> None:
        self.check(suite(case("test_a"), case("test_b")), "missing scenario: test_c")

    def test_a_duplicate_fails(self) -> None:
        self.check(PASS.replace("</testsuite>", case("test_a") + "</testsuite>"), "duplicate", "test_a")

    def test_an_extra_testcase_fails(self) -> None:
        self.check(PASS.replace("</testsuite>", case("test_z") + "</testsuite>"), "unexpected", "test_z")

    def test_a_renamed_scenario_fails_both_ways(self) -> None:
        self.check(PASS.replace("test_c", "test_c2"), "missing scenario: test_c", "test_c2")

    def test_a_skip_fails(self) -> None:
        skipped = '<skipped type="pytest.skip" message="no grant"/>'
        self.check(suite(case("test_a"), case("test_b", skipped), case("test_c")), "test_b", "skipped")

    def test_an_xfail_fails(self) -> None:
        # pytest's junitxml records an expected failure as a <skipped> child.
        xfail = '<skipped type="pytest.xfail" message="known"/>'
        self.check(suite(case("test_a"), case("test_b"), case("test_c", xfail)), "test_c", "skipped")

    def test_a_failure_fails(self) -> None:
        self.check(suite(case("test_a", '<failure message="boom"/>'), case("test_b"), case("test_c")), "failure")

    def test_a_setup_error_beside_a_pass_fails(self) -> None:
        # pytest writes a passed call and a setup/teardown error as two
        # <testcase> entries with the same name.
        error = '<error message="failed on setup with fixture"/>'
        xml = suite(case("test_a"), case("test_b"), case("test_c"), case("test_b", error))
        self.check(xml, "<error>", "duplicate")

    def test_a_teardown_error_inside_the_testcase_fails(self) -> None:
        error = '<error message="failed on teardown"/>'
        self.check(suite(case("test_a"), case("test_b", error), case("test_c")), "test_b", "<error>")

    def test_malformed_xml_fails(self) -> None:
        self.check("<testsuite><testcase", "not parseable")

    def test_an_empty_file_fails(self) -> None:
        self.check("", "not parseable")

    def test_a_foreign_root_fails(self) -> None:
        self.check("<html/>", "unexpected root")

    def test_an_empty_suite_fails(self) -> None:
        self.check("<testsuite/>", "missing scenario")

    def test_an_empty_expected_list_fails(self) -> None:
        self.assertTrue(junit_guard.problems(suite(), []))


class MainTests(unittest.TestCase):
    def run_main(self, xml: str | None) -> int:
        with tempfile.TemporaryDirectory() as tmp:
            scenarios = Path(tmp) / "test_x.py"
            scenarios.write_text(
                'SCENARIOS = ["test_a", "test_b", "test_c"]\n'
                "def test_a(): pass\ndef test_b(): pass\ndef test_c(): pass\n"
            )
            report = Path(tmp) / "junit.xml"
            if xml is not None:
                report.write_text(xml)
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                return junit_guard.main([str(report), "--scenarios", str(scenarios)])

    def test_exit_zero_on_a_clean_report(self) -> None:
        self.assertEqual(self.run_main(PASS), 0)

    def test_exit_one_on_a_bad_report(self) -> None:
        self.assertEqual(self.run_main(suite(case("test_a"))), 1)

    def test_exit_one_when_the_file_is_missing(self) -> None:
        self.assertEqual(self.run_main(None), 1)


class ScenarioExtractionTests(unittest.TestCase):
    def test_the_real_module_lists_its_scenarios(self) -> None:
        listed, tests = junit_guard.scenarios_and_tests(ROOSTTEST_DIR / "test_real_input_mac.py")
        self.assertEqual(listed, tests)
        self.assertTrue(listed)


if __name__ == "__main__":
    unittest.main()
