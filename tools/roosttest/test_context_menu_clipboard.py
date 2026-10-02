"""Copy Path, from a row's right-click menu (plan 073 D9, #338).

The item writes the row's path to the system clipboard through the
clipboard queue, so this reads it back with `clipboard.dump` — which
needs a clipboard the harness can own: the X11 lane and the macOS cell,
not headless Wayland (`ICED_CLIPBOARD_TESTS`). The rest of the menu is
`test_context_menu.py`.

Skipped under `--roost-target mac`: the Swift app has no context-menu ops.
"""

from __future__ import annotations

import os

import pytest
from test_osc52 import _seed_baseline, _wait_clipboard
from util import wait_tab_attached

TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"

#: A child that never reports a directory of its own, so the tab's path
#: stays the one it was opened in.
QUIET_ARGV = ["/bin/sh", "-c", "exec sleep 300"]

pytestmark = pytest.mark.skipif(
    not TEST_MODE,
    reason="the app.context_menu_* ops require ROOST_TEST_MODE=1",
)


@pytest.fixture(autouse=True)
def _iced_only(target):
    if target != "iced":
        pytest.skip("the context-menu ops are iced's; Roost.app answers unknown-op")


def _copied(roost, target: dict, action: str, expected: str) -> None:
    _seed_baseline(roost, "system")
    roost.context_menu_activate(target, action)
    _wait_clipboard(roost, "system", expected)


def test_copy_path_puts_the_rows_path_on_the_clipboard(roost, project):
    tab = roost.open_tab(project, cwd="/tmp", title="copy-path", argv=QUIET_ARGV)
    wait_tab_attached(roost, tab)
    _copied(roost, {"tab_id": str(tab)}, "copy_tab_path", roost.tab(tab)["cwd"])
    _copied(roost, {"project_id": str(project)}, "copy_project_path", roost.project(project)["cwd"])
