"""Dock-badge E2E — plan 027 C7, the macOS native seam's first consumer.

The iced UI mirrors its notification-inbox count onto
`NSApp.dockTile.badgeLabel` (`crates/roost-iced/src/macos/dock_badge.rs`),
the parity port of `mac/Sources/Roost/App.swift`'s `refreshDockBadge()` —
the count as a decimal string, `nil` at zero. The write rides the
reconcile in the iced update loop, so it lands asynchronously; every
assertion here is a condition wait, never a sleep.

`app.dock_badge` reads the label back off AppKit rather than re-deriving
it from the inbox, so what these tests assert is that the write actually
reached the Dock — not merely that the count→label mapping is right
(`dock_badge::label`'s unit tests pin that).

macOS-iced-only: see `_mac_iced_only`. Nothing here needs the Dock to be
*visible* — a locked screen or an occluded window changes none of it.
"""

from __future__ import annotations

import os
import sys

import pytest

TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"


@pytest.fixture(autouse=True)
def _mac_iced_only(target):
    """Skip unless BOTH halves hold.

    `make e2e-mac` runs this whole directory on macOS, so an OS-only skip
    would aim an iced-only op at the Swift app (whose dispatcher answers
    `unknown-op`). And the iced UI also builds for Linux, where there is
    no Dock at all — so a target-only skip would fail every Linux lane.
    """
    if sys.platform != "darwin" or target != "iced":
        pytest.skip("app.dock_badge is macOS-iced-only (roadmap M6 § 6b seam)")


def _wait_badge(roost, want: str | None, what: str) -> None:
    roost._wait(lambda: roost.app_dock_badge() == want, 5.0, what)


def _drain_inbox(palette) -> None:
    """Empty the inbox through the user-facing "Clear All" command.

    The badge is app-global state, so a pending notification an earlier
    module left behind would poison the baseline. Clear-all drives the
    same false-edge a user's triage does.
    """
    palette.palette_open()
    palette.palette_activate("clear_notifications")


@pytest.mark.skipif(
    not TEST_MODE,
    reason="app.dock_badge requires ROOST_TEST_MODE=1 in the UI's launch env",
)
class TestDockBadge:
    def test_badge_appears_for_a_pending_notification_and_clears(
        self, roost, project, palette
    ):
        _drain_inbox(palette)
        _wait_badge(roost, None, "baseline: an empty inbox leaves no badge")

        a = roost.open_tab(project, cwd="/tmp")
        roost.open_tab(project, cwd="/tmp")  # steals active, so `a` is background
        # Notification policy B suppresses a raise for the active tab of an
        # active window, so the notified tab must be a background one.
        roost.notify(a, "DockBadge", "pending")
        roost.wait_notification(a, True)
        _wait_badge(roost, "1", "one pending notification badges the Dock tile")

        # The same false-edge the UI's focus-and-clear drives.
        roost.clear_notification(a)
        roost.wait_notification(a, False)
        _wait_badge(roost, None, "the badge clears when the inbox empties")

    def test_badge_counts_every_pending_tab(self, roost, project, palette):
        """The badge is the count, not a presence dot — the property that
        makes `label()`'s decimal formatting load-bearing."""
        _drain_inbox(palette)
        _wait_badge(roost, None, "baseline: an empty inbox leaves no badge")

        a = roost.open_tab(project, cwd="/tmp")
        b = roost.open_tab(project, cwd="/tmp")
        roost.open_tab(project, cwd="/tmp")  # steals active
        roost.notify(a, "DockBadge", "first")
        roost.notify(b, "DockBadge", "second")
        roost.wait_notification(a, True)
        roost.wait_notification(b, True)
        _wait_badge(roost, "2", "two pending tabs badge the Dock tile as 2")

        _drain_inbox(palette)
        _wait_badge(roost, None, "clear-all drops the badge")
