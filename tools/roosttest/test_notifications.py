"""Notification-routing E2E — the multi-project notification inbox is
roost's differentiator. Drives `notification.create` + the palette
`view_notifications` inbox frame (list, jump-to-notification, clear-all)
on either UI. Basic per-tab badge set/clear lives in the smoke suite;
this exercises the inbox surface a user actually triages through.
"""

from __future__ import annotations


def _wait(roost, pred, what, timeout=4.0):
    roost._wait(pred, timeout, what)


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
    row per pending notification, carrying its title + body."""
    a = roost.open_tab(project, cwd="/tmp")
    b = roost.open_tab(project, cwd="/tmp")
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


def test_jump_to_notification_focuses_and_clears(roost, project, palette):
    """Activating an inbox row jumps to that tab (the triage action) and
    clears its badge — closing the palette."""
    a = roost.open_tab(project, cwd="/tmp")
    b = roost.open_tab(project, cwd="/tmp")  # b is active
    roost.notify(a, "JumpMe")
    assert roost.identify()["active_tab_id"] == b
    # Wait until the inbox registers a before navigating to jump (it lags
    # the badge on Mac; the frame snapshots at push).
    roost._wait(lambda: f"notif:{a}" in _inbox_ids(palette), 5.0, "inbox registers a")

    palette.palette_open()
    palette.palette_activate("view_notifications")
    st = palette.palette_activate(f"notif:{a}")
    assert st["open"] is False  # jumping confirms + closes the palette
    # The jump updates the *core* active tab (not just UI selection), so
    # identify reflects where the user was sent.
    _wait(roost, lambda: roost.identify()["active_tab_id"] == a, "jumped to a (core active)")
    _wait(roost, lambda: roost.tab(a).get("has_notification") is False, "a badge cleared by jump")


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
