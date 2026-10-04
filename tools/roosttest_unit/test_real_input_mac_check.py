"""`runner.readiness_report` blocks only on the environment, never on claimants (plan 075 §D4.4)."""

from __future__ import annotations

import contextlib
import copy
import io
import sys
import unittest
from unittest import mock
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "input" / "mac"))

import runner  # noqa: E402

READY = {
    "capabilities": {"accessibility": True, "listen_event": True, "post_event": True, "screen_capture": True},
    "console": {"available": True, "locked": False, "on_console": True},
    "session": {"available": True, "locked": False, "on_console": True},
    "secure_input_pid": None,
    "input_source": "com.apple.keylayout.US",
    "frontmost": {"pid": 10, "bundle_id": "com.example.term"},
    "displays": [
        {"id": 1, "main": True, "bounds": {"width": 1470.0, "height": 956.0}, "safe_area_top": 32.0, "menu_bar_inset": 33.0}
    ],
}
BLOCKING = {"claimants": [{"pid": 7, "claims_key": True, "position": "ahead", "ahead": True, "blocks": True}]}


def variant(**changes):
    report = copy.deepcopy(READY)
    for path, value in changes.items():
        node = report
        *parents, leaf = path.split("__")
        for key in parents:
            node = node[key]
        node[leaf] = value
    return report


class ReadinessReportTests(unittest.TestCase):
    def test_a_ready_desktop_has_no_blocker(self) -> None:
        lines, blocker = runner.readiness_report(READY, {"claimants": []})
        self.assertIsNone(blocker)
        self.assertTrue(any("menu_bar_inset=33.0" in line for line in lines))

    def test_claimants_are_information_only(self) -> None:
        lines, blocker = runner.readiness_report(READY, BLOCKING)
        self.assertIsNone(blocker)
        text = "\n".join(lines)
        self.assertIn("claimant pid 7", text)
        self.assertIn("information only", text)

    def test_each_environment_blocker_blocks(self) -> None:
        cases = {
            "grant": variant(capabilities__post_event=False),
            "lock": variant(session__locked=True),
            "off console": variant(console__on_console=False),
            "secure input": variant(secure_input_pid=42),
            "layout": variant(input_source="com.apple.keylayout.German"),
        }
        for name, report in cases.items():
            with self.subTest(name):
                self.assertIsNotNone(runner.readiness_report(report, None)[1])

    def test_an_unavailable_session_reports_the_gui_session_blocker(self) -> None:
        report = variant(session={"available": False})
        lines, blocker = runner.readiness_report(report, None)
        self.assertEqual(blocker, "the helper is outside the GUI login session")
        self.assertTrue(any("available=False" in line for line in lines))

    def test_check_exits_nonzero_without_a_traceback_when_unavailable(self) -> None:
        report = variant(session={"available": False})

        class FakeHelper:
            def __init__(self, _artifacts) -> None:
                pass

            def __enter__(self):
                return self

            def __exit__(self, *_exc) -> None:
                return None

            def preflight(self):
                return report

            def claimants(self, _pid):
                return {"claimants": []}

        out = io.StringIO()
        with mock.patch.object(runner, "Helper", FakeHelper), contextlib.redirect_stdout(out):
            code = runner.check(Path("."))
        self.assertEqual(code, 1)
        self.assertIn("outside the GUI login session", out.getvalue())


if __name__ == "__main__":
    unittest.main()
