"""A desktop banner click raises the window (#351, plan 074 §D6).

`app.notification_activate` clicks a tab's banner as the notification
server would, with or without the spec 1.2 activation token, and
`app.last_activation` reads back what the raise came to. The expected
outcome is worked out from the display the lane runs on and from what the
compositor's registry reports, not written in per lane: Xvfb comes to
`not-wayland`, weston to `no-global`. Either way the click focuses its tab.

Skipped on macOS, whose banners carry no token, and under
`--roost-target mac`: the Swift app has no such ops.
"""

from __future__ import annotations

import os
import sys
import uuid

import pytest

TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"

#: How many clicks a slow compositor's registry gets to answer in.
CLICKS_FOR_THE_REGISTRY = 3

pytestmark = [
    pytest.mark.skipif(
        not TEST_MODE, reason="the raise test ops require ROOST_TEST_MODE=1"
    ),
    pytest.mark.skipif(
        sys.platform == "darwin", reason="a macOS banner carries no activation token"
    ),
]


@pytest.fixture(autouse=True)
def _iced_only(target):
    if target != "iced":
        pytest.skip("the raise test ops are iced's; Roost.app answers unknown-op")


@pytest.fixture(autouse=True)
def _window_open(_iced_only, roost):
    """The UI answers IPC before its window opens, and a click that lands
    first raises nothing, so it settles nothing either. A screenshot is
    served only once there is a window to capture."""
    roost.screenshot()


def click_and_settle(roost, tab: int, token: str | None) -> dict:
    """Click `tab`'s banner and return the raise it settled to. Every click
    in this module is told apart by its token, so a click without one has
    to follow a click with one."""
    roost.notification_activate(tab, token)
    seen: dict = {}

    def settled() -> bool:
        seen.update(roost.last_activation())
        return seen["outcome"] is not None and seen["token"] == token

    roost._wait(settled, 5.0, f"the raise for token {token!r} settles")
    return seen


def expected_outcome(settled: dict) -> str:
    """What a click with a token should come to on this lane's display,
    given what the registry reported."""
    if not os.environ.get("WAYLAND_DISPLAY"):
        return "not-wayland"
    assert settled["activation_global"] is not None, (
        f"a Wayland compositor whose registry never answered: {settled}"
    )
    return "activated" if settled["activation_global"] else "no-global"


def test_a_click_with_a_token_raises_by_what_the_compositor_offers(roost, project):
    clicked = roost.open_tab(project, cwd="/tmp")
    other = roost.open_tab(project, cwd="/tmp")
    assert roost.identify()["active_tab_id"] == other

    for _ in range(CLICKS_FOR_THE_REGISTRY):
        settled = click_and_settle(roost, clicked, f"t-{uuid.uuid4().hex}")
        if settled["outcome"] != "failed":
            break

    assert settled["outcome"] == expected_outcome(settled), settled
    roost._wait(
        lambda: roost.identify()["active_tab_id"] == clicked,
        5.0,
        "the click focuses its tab",
    )


def test_a_click_without_a_token_has_nothing_to_spend_and_still_focuses(roost, project):
    clicked = roost.open_tab(project, cwd="/tmp")
    other = roost.open_tab(project, cwd="/tmp")
    click_and_settle(roost, other, f"t-{uuid.uuid4().hex}")
    assert roost.identify()["active_tab_id"] == other

    settled = click_and_settle(roost, clicked, None)

    assert settled == {"outcome": "no-token", "token": None, "activation_global": None}
    roost._wait(
        lambda: roost.identify()["active_tab_id"] == clicked,
        5.0,
        "the click focuses its tab",
    )
