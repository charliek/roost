"""Notification-routing E2E — the multi-project notification inbox is
roost's differentiator. Drives `notification.create` + the palette
`view_notifications` inbox frame (list, jump-to-notification, clear-all)
on either UI. Basic per-tab badge set/clear lives in the smoke suite;
this exercises the inbox surface a user actually triages through.
"""

from __future__ import annotations

import pytest

from client import Timeout


def _wait(roost, pred, what, timeout=4.0):
    roost._wait(pred, timeout, what)


def _delivered(roost, tab_id, timeout=2.0) -> bool:
    """Whether a notification for `tab_id` produced a badge within a
    bounded wait. Used where the ANSWER is the assertion (policy B's
    suppression), so a negative result must not raise."""
    try:
        roost.wait_notification(tab_id, True, timeout=timeout)
        return True
    except Timeout:
        return False


def _inbox_ids(palette):
    """Snapshot the inbox by (re)pushing the `view_notifications` frame.
    A palette frame fixes its rows at push time, so re-pushing is how you
    read the *current* inbox. Leaves the palette closed."""
    palette.palette_open()
    st = palette.palette_activate("view_notifications")
    ids = palette.palette_item_ids(st)
    palette.palette_dismiss()
    return ids


def test_inbox_lists_pending_via_palette(roost, project, palette):
    """`view_notifications` drills into the inbox frame, one `notif:<tab>`
    row per pending notification, carrying its title + body.

    Both notified tabs are background tabs (the third steals active):
    under notification policy B (plan 002 §3.5) a notification for the
    active tab of an active window produces no inbox row at all, so
    notifying the active tab would make this depend on window focus."""
    a = roost.open_tab(project, cwd="/tmp")
    b = roost.open_tab(project, cwd="/tmp")
    roost.open_tab(project, cwd="/tmp")  # steals active
    roost.notify(a, "AlphaBuild", "passed")
    roost.notify(b, "BetaBuild", "failed")
    # The inbox populates via an event that can lag the workspace badge,
    # and the frame snapshots its rows at push — so wait until both
    # register, re-pushing each poll, before reading details.
    roost._wait(
        lambda: {f"notif:{a}", f"notif:{b}"} <= set(_inbox_ids(palette)),
        5.0,
        "inbox lists both tabs",
    )

    palette.palette_open()
    st = palette.palette_activate("view_notifications")
    assert st["frame"] == "notifications"
    by_id = {it["id"]: it for it in st["items"]}
    assert f"notif:{a}" in by_id and f"notif:{b}" in by_id, list(by_id)
    # The row title is the "<project> · <tab>" context (so triage shows
    # *where*); the body is the subtitle. The notification's own title is
    # the desktop-banner title, not surfaced in the inbox.
    assert "·" in by_id[f"notif:{a}"]["title"], by_id[f"notif:{a}"]
    assert by_id[f"notif:{a}"].get("subtitle") == "passed"


def test_jump_to_notification_focuses_and_clears(roost, project, palette, target):
    """Activating an inbox row jumps to that tab (the triage action) and
    clears its badge — closing the palette.

    GTK-only extra (plan 005 §3.11): `ensure_sidebar_visible()` was added
    to the agent-row jump and retrofitted onto this pre-existing
    notification-jump path (a cross-UI divergence from Mac's
    `ensureSidebarVisible()` this plan's review surfaced), so a jump from
    a collapsed sidebar must reveal it."""
    a = roost.open_tab(project, cwd="/tmp")
    b = roost.open_tab(project, cwd="/tmp")  # b is active
    roost.notify(a, "JumpMe")
    assert roost.identify()["active_tab_id"] == b
    # Wait until the inbox registers a before navigating to jump (it lags
    # the badge on Mac; the frame snapshots at push).
    roost._wait(lambda: f"notif:{a}" in _inbox_ids(palette), 5.0, "inbox registers a")

    if target == "gtk":
        if not roost.window_metrics()["sidebar_collapsed"]:
            palette.palette_open()
            palette.palette_activate("toggle_sidebar")
            roost._wait(
                lambda: roost.window_metrics()["sidebar_collapsed"], 5.0, "sidebar collapses"
            )

    palette.palette_open()
    palette.palette_activate("view_notifications")
    st = palette.palette_activate(f"notif:{a}")
    assert st["open"] is False  # jumping confirms + closes the palette
    # The jump updates the *core* active tab (not just UI selection), so
    # identify reflects where the user was sent.
    _wait(roost, lambda: roost.identify()["active_tab_id"] == a, "jumped to a (core active)")
    _wait(roost, lambda: roost.tab(a).get("has_notification") is False, "a badge cleared by jump")

    if target == "gtk":
        _wait(
            roost,
            lambda: not roost.window_metrics()["sidebar_collapsed"],
            "sidebar reappears after the notification jump (§3.11)",
        )


def test_jump_to_unread_focuses_notified_tab(roost, project, palette):
    """`jump_to_unread` (Cmd/Ctrl+Shift+U) focuses the next tab with a
    pending notification + clears it — a multi-project triage shortcut.
    Now on both UIs (was Mac-only)."""
    a = roost.open_tab(project, cwd="/tmp")
    b = roost.open_tab(project, cwd="/tmp")  # b active
    roost.notify(a, "Unread")
    assert roost.identify()["active_tab_id"] == b
    roost._wait(lambda: f"notif:{a}" in _inbox_ids(palette), 5.0, "inbox registers a")

    palette.palette_open()
    palette.palette_activate("jump_to_unread")  # → focuses the unread tab
    _wait(roost, lambda: roost.identify()["active_tab_id"] == a, "jumped to unread a")
    _wait(roost, lambda: roost.tab(a).get("has_notification") is False, "a cleared by jump")


def test_focused_active_tab_gets_no_badge_and_no_inbox_row(roost, project, palette):
    """Notification policy B (plan 002 §3.5): `suppress := window_active
    && tab_active`. When it holds there is no badge AND no inbox row —
    the two are coupled because inbox membership derives from the same
    `has_notification` bit. A notification for the tab you are actively
    looking at is considered seen.

    Both halves of the predicate are asserted: a background tab in the
    same window always gets both, and (when the environment gives the
    window focus at all) the active tab gets neither — including after
    the user switches away, which must not retroactively produce a badge.
    """
    bg = roost.open_tab(project, cwd="/tmp")
    active = roost.open_tab(project, cwd="/tmp")  # steals active
    _wait(roost, lambda: roost.identify()["active_tab_id"] == active, "second tab is active")

    # tab_active is false -> never suppressed, whatever the window is doing.
    roost.notify(bg, "Background", "delivered")
    assert _delivered(roost, bg), "a background tab's notification must always land"
    roost._wait(lambda: f"notif:{bg}" in _inbox_ids(palette), 5.0, "inbox lists the background tab")

    roost.notify(active, "Foreground", "should be suppressed")
    if _delivered(roost, active):
        # The window half of the predicate is false: a headless GTK UI
        # under xvfb (no window manager) never becomes active, so the
        # suppression arm is genuinely unobservable here. Not a silent
        # gap — the full four-way matrix is covered by the workspace
        # unit tests on both UIs.
        pytest.skip(
            "the UI window is not active in this environment, so the "
            "`window_active` half of policy B can't be exercised "
            "[alt-coverage: daemon::state attention_policy_* (Rust) + "
            "WorkspaceStateTests.attentionPolicy* (Swift)]"
        )

    assert roost.has_notification(active) is False
    assert f"notif:{active}" not in _inbox_ids(palette)

    # Switching away must not resurrect it: the raise was dropped
    # entirely, so there is no pending bit to re-evaluate later.
    roost.focus(bg)
    _wait(roost, lambda: roost.identify()["active_tab_id"] == bg, "switched away from `active`")
    assert not _delivered(roost, active, timeout=1.0), "no retroactive badge"
    assert f"notif:{active}" not in _inbox_ids(palette)


def test_clear_all_empties_inbox(roost, project, palette):
    """`clear_notifications` empties the inbox + drops every badge; the
    frame then shows only the empty sentinel."""
    a = roost.open_tab(project, cwd="/tmp")
    roost.open_tab(project, cwd="/tmp")
    roost.notify(a, "Transient")

    # Wait until the inbox actually registers it before clearing: the
    # inbox populates via an event that can lag the workspace badge, and
    # "Clear All" iterates the inbox — clearing before it registers would
    # miss the tab. Re-push the frame each poll (it snapshots at push).
    roost._wait(lambda: f"notif:{a}" in _inbox_ids(palette), 5.0, "inbox registers a")

    palette.palette_open()
    palette.palette_activate("clear_notifications")  # Clear All → closes palette
    _wait(roost, lambda: roost.tab(a).get("has_notification") is False, "badge cleared by clear-all")

    # Inbox drains to the empty sentinel (row removal rides the same
    # false-edge event as the badge clear, so it can lag a tick).
    roost._wait(lambda: _inbox_ids(palette) == ["notif:none"], 5.0, "inbox drained to sentinel")
