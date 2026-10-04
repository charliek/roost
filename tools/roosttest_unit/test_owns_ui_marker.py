"""conftest's `owns_ui` marker (plan 074 §D7): the shared UI launcher never runs
for a module that launches its own UI.

`_ui_session` is session-scoped and autouse, so a module's own fixtures cannot
opt out of it; the marker is how one does, including under an inherited
`ROOST_TEST_FRESH=1`.

The decision is `ui.needs_shared_ui`, tested directly. The wiring is proven by
running pytest on a marked module against the real conftest with the
launcher's entry points replaced by recorders — when the interpreter running
these tests has pytest (CI's bare `harness-unit` interpreter does not, and
skips that half).
"""

from __future__ import annotations

import importlib.util
import os
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

ROOSTTEST_DIR = Path(__file__).resolve().parents[1] / "roosttest"
sys.path.insert(0, str(ROOSTTEST_DIR))

import ui  # noqa: E402


class NeedsSharedUiTests(unittest.TestCase):
    def test_tests_that_own_their_ui_or_need_none_need_no_shared_ui(self) -> None:
        self.assertFalse(ui.needs_shared_ui([["owns_ui"], ["owns_ui", "parametrize"]]))
        self.assertFalse(ui.needs_shared_ui([["session_daemon"], ["owns_ui"]]))
        self.assertFalse(ui.needs_shared_ui([]))

    def test_any_other_test_still_gets_the_shared_ui(self) -> None:
        self.assertTrue(ui.needs_shared_ui([["owns_ui"], []]))
        self.assertTrue(ui.needs_shared_ui([["host_client"]]))
        self.assertTrue(ui.needs_shared_ui([["skipif", "owns_ui_not"]]))


# Loads the real conftest after replacing the shared launcher's entry points
# with recorders, then re-exports its hooks and fixtures from this conftest.
_CONFTEST = """
import importlib.util
import sys

sys.path.insert(0, {roosttest!r})
import ui


def _recorder(name):
    def record(*args, **kwargs):
        with open({calls!r}, "a") as calls:
            calls.write(name + "\\n")
        return False

    return record


for _name in ("start_session", "end_session", "launch", "quit"):
    setattr(ui, _name, _recorder(_name))

_spec = importlib.util.spec_from_file_location("roosttest_conftest", {conftest!r})
_real = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_real)
globals().update({{key: value for key, value in vars(_real).items() if not key.startswith("__")}})


def pytest_configure(config):
    config.addinivalue_line("markers", "owns_ui: launches its own UI")
"""

_MARKED = """
import pytest

pytestmark = pytest.mark.owns_ui


def test_owns_its_ui():
    pass
"""

_UNMARKED = """
def test_uses_the_shared_ui():
    pass
"""


@unittest.skipIf(importlib.util.find_spec("pytest") is None, "this interpreter has no pytest")
class ConftestWiringTests(unittest.TestCase):
    def setUp(self) -> None:
        self.root = Path(tempfile.mkdtemp(prefix="owns-ui-"))
        self.addCleanup(shutil.rmtree, self.root, True)
        self.calls = self.root / "calls"
        (self.root / "conftest.py").write_text(
            _CONFTEST.format(
                roosttest=str(ROOSTTEST_DIR),
                calls=str(self.calls),
                conftest=str(ROOSTTEST_DIR / "conftest.py"),
            )
        )
        (self.root / "test_marked.py").write_text(textwrap.dedent(_MARKED))
        (self.root / "test_unmarked.py").write_text(textwrap.dedent(_UNMARKED))

    def run_pytest(self, *modules: str) -> list[str]:
        self.calls.write_text("")
        env = {key: value for key, value in os.environ.items() if key != "PYTEST_ADDOPTS"}
        env["ROOST_TEST_FRESH"] = "1"
        env["PYTHONDONTWRITEBYTECODE"] = "1"
        result = subprocess.run(
            [
                sys.executable, "-m", "pytest", "-q", "-p", "no:cacheprovider",
                "--rootdir", str(self.root), "--roost-fresh", "--roost-target", "iced",
                *(str(self.root / module) for module in modules),
            ],
            cwd=self.root,
            env=env,
            capture_output=True,
            text=True,
            check=False,
            timeout=120,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return self.calls.read_text().splitlines()

    def test_the_launcher_stands_down_for_a_marked_module_even_when_fresh(self) -> None:
        self.assertEqual(self.run_pytest("test_marked.py"), [])

    def test_a_mixed_run_still_launches_for_the_rest(self) -> None:
        # Also what proves the recorders see the launcher at all.
        self.assertEqual(self.run_pytest("test_marked.py", "test_unmarked.py"), ["start_session"])


if __name__ == "__main__":
    unittest.main()
