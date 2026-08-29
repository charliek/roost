//! The versioned event channel delivers one message per commit.
//!
//! This is what makes a subscriber's gap check sufficient: it can never
//! observe half of a commit, and every revision it skips is a real loss
//! rather than an event-free commit it was never told about.

use roost_engine::{Workspace, WorkspaceEvent};

#[tokio::test]
async fn a_multi_event_commit_arrives_as_one_message_in_order() {
    let workspace = Workspace::new();
    let project = workspace.create_project("p", "/tmp").unwrap();
    let first = workspace.open_tab(project.id, "/tmp", "one").unwrap();
    let second = workspace.open_tab(project.id, "/tmp", "two").unwrap();
    let mut events = workspace.subscribe_versioned();

    // `delete_project` is the widest commit the workspace makes: both
    // tabs close, the project goes, and the active selection clears.
    workspace.delete_project(project.id).unwrap();

    let message = events.recv().await.unwrap();
    assert_eq!(message.revision, 4);
    assert_eq!(message.events.len(), 4);
    assert!(matches!(
        message.events[0],
        WorkspaceEvent::TabClosed { tab_id } if tab_id == first.id
    ));
    assert!(matches!(
        message.events[1],
        WorkspaceEvent::TabClosed { tab_id } if tab_id == second.id
    ));
    assert!(matches!(
        message.events[2],
        WorkspaceEvent::ProjectDeleted { project_id } if project_id == project.id
    ));
    assert!(matches!(
        message.events[3],
        WorkspaceEvent::ActiveChanged {
            project_id: 0,
            tab_id: 0
        }
    ));

    assert!(
        events.try_recv().is_err(),
        "the commit must publish exactly one versioned message"
    );
}

#[tokio::test]
async fn an_event_free_commit_still_publishes_its_revision() {
    let workspace = Workspace::new();
    let mut events = workspace.subscribe_versioned();

    // `ensure_default_project` commits with no events when a default
    // project already exists — the revision still moves, so the message
    // must still be sent or the stream grows an unexplained gap.
    let first = workspace.ensure_default_project("/tmp");
    let opening = events.recv().await.unwrap();
    assert!(
        !opening.events.is_empty(),
        "creating the default project emits events"
    );

    let again = workspace.ensure_default_project("/tmp");
    assert_eq!(again, first, "the second call reuses the same project");
    let fence = events.recv().await.unwrap();
    assert!(
        fence.events.is_empty(),
        "expected an event-free commit, got {:?}",
        fence.events
    );
    assert_eq!(fence.revision, opening.revision + 1);
}

#[tokio::test]
async fn the_revision_stream_advances_by_exactly_one_per_commit() {
    let workspace = Workspace::new();
    let mut events = workspace.subscribe_versioned();

    let project = workspace.create_project("p", "/tmp").unwrap();
    for i in 0..8 {
        workspace
            .open_tab(project.id, "/tmp", &format!("t{i}"))
            .unwrap();
    }
    workspace.delete_project(project.id).unwrap();

    for expected in 1..=10 {
        let message = events.recv().await.unwrap();
        assert_eq!(message.revision, expected);
    }
    assert!(events.try_recv().is_err(), "no extra messages");
}
