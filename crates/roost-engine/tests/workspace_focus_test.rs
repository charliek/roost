//! Focusing a tab acknowledges its notification (#369).
//!
//! The acknowledgement rides in `focus_tab`'s own commit, so every
//! surface that focuses — a click, a keybind, a palette jump, a banner,
//! IPC `tab.focus`, the facade — clears the badge by construction. The
//! versioned stream delivers one message per commit, which is what makes
//! "same commit, in order" assertable here: a subscriber that sees the
//! pair in one message could not have seen them split.

use roost_engine::{Workspace, WorkspaceEvent};

#[tokio::test]
async fn focus_tab_clears_the_focused_tabs_notification_in_one_commit() {
    let workspace = Workspace::new();
    let project = workspace.create_project("p", "/tmp").unwrap();
    let badged = workspace.open_tab(project.id, "/tmp", "badged").unwrap();
    let other = workspace.open_tab(project.id, "/tmp", "other").unwrap();
    workspace.focus_tab(other.id).unwrap();
    workspace.set_tab_has_notification(badged.id, true).unwrap();

    let mut events = workspace.subscribe_versioned();
    workspace.focus_tab(badged.id).unwrap();

    let message = events.recv().await.unwrap();
    assert_eq!(
        message.events.len(),
        2,
        "the focus and the acknowledgement are one commit: {:?}",
        message.events
    );
    assert!(matches!(
        message.events[0],
        WorkspaceEvent::ActiveChanged { project_id, tab_id }
            if project_id == project.id && tab_id == badged.id
    ));
    assert!(matches!(
        message.events[1],
        WorkspaceEvent::TabNotification {
            tab_id,
            has_pending: false
        } if tab_id == badged.id
    ));
    let extra = events.try_recv();
    assert!(extra.is_err(), "one revision step, not two: {extra:?}");
    assert!(!workspace.tab(badged.id).unwrap().has_notification);
}

#[tokio::test]
async fn refocusing_a_clear_tab_emits_no_notification_event() {
    let workspace = Workspace::new();
    let project = workspace.create_project("p", "/tmp").unwrap();
    let one = workspace.open_tab(project.id, "/tmp", "one").unwrap();
    let two = workspace.open_tab(project.id, "/tmp", "two").unwrap();
    workspace.focus_tab(two.id).unwrap();

    let mut events = workspace.subscribe_versioned();
    workspace.focus_tab(one.id).unwrap();

    let message = events.recv().await.unwrap();
    assert_eq!(
        message.events.len(),
        1,
        "a tab with nothing pending earns no notification edge: {:?}",
        message.events
    );
    assert!(matches!(
        message.events[0],
        WorkspaceEvent::ActiveChanged { tab_id, .. } if tab_id == one.id
    ));
}

#[tokio::test]
async fn focusing_the_already_active_tab_still_acknowledges_its_notification() {
    let workspace = Workspace::new();
    let project = workspace.create_project("p", "/tmp").unwrap();
    // A notification raised while the window is unfocused is not
    // suppressed, so the active tab can wear a badge and the user can
    // click the pill it is already on.
    let active = workspace.open_tab(project.id, "/tmp", "active").unwrap();
    workspace.focus_tab(active.id).unwrap();
    workspace.set_tab_has_notification(active.id, true).unwrap();

    let mut events = workspace.subscribe_versioned();
    workspace.focus_tab(active.id).unwrap();

    let message = events.recv().await.unwrap();
    assert_eq!(message.events.len(), 2, "{:?}", message.events);
    assert!(matches!(
        message.events[0],
        WorkspaceEvent::ActiveChanged { tab_id, .. } if tab_id == active.id
    ));
    assert!(matches!(
        message.events[1],
        WorkspaceEvent::TabNotification {
            tab_id,
            has_pending: false
        } if tab_id == active.id
    ));
    assert!(!workspace.tab(active.id).unwrap().has_notification);
}

/// Deliberate, and the counterpart to the three above: a selection the
/// user did not ask for is not an acknowledgement. Closing the active
/// tab dumps the user onto a neighbour, and clearing that neighbour's
/// notification would eat it — the badge, the rollup and the inbox row
/// with it — for something the user never focused. `close_tab` builds
/// its own commit and never reaches `focus_tab`; that is the design,
/// not an oversight.
#[tokio::test]
async fn closing_the_active_tab_does_not_acknowledge_the_fallback_tabs_notification() {
    let workspace = Workspace::new();
    let project = workspace.create_project("p", "/tmp").unwrap();
    let fallback = workspace.open_tab(project.id, "/tmp", "fallback").unwrap();
    let active = workspace.open_tab(project.id, "/tmp", "active").unwrap();
    workspace.focus_tab(active.id).unwrap();
    workspace
        .set_tab_has_notification(fallback.id, true)
        .unwrap();

    workspace.close_tab(active.id).unwrap();

    assert_eq!(workspace.active(), (project.id, fallback.id));
    assert!(
        workspace.tab(fallback.id).unwrap().has_notification,
        "the fallback tab keeps its notification: it was selected for the user, not by them"
    );
}
