//! Validates the two facts the event bridge glues together for the
//! issue #79 Lagged→resync path: overflowing the workspace broadcast
//! buffer produces a `Lagged` error, and `resync_event()` carries the
//! full current state for the UI to reconcile against.

use std::sync::Arc;

use roost_linux::daemon::{Workspace, WorkspaceEvent};
use tokio::sync::broadcast::error::TryRecvError;

/// `EVENT_CHANNEL_CAPACITY` in `daemon/state.rs`. Sending more than
/// this without draining forces the subscriber to lag.
const CHANNEL_CAPACITY: usize = 256;

#[test]
fn overflowing_broadcast_lags_and_resync_event_carries_full_snapshot() {
    let ws = Arc::new(Workspace::new());
    // Subscribe, then never drain — every create pushes one
    // `ProjectCreated` event into the bounded buffer.
    let mut rx = ws.subscribe();

    let total = CHANNEL_CAPACITY + 50;
    for i in 0..total {
        ws.create_project(&format!("p{i}"), "/tmp")
            .expect("create_project");
    }

    // The receiver's cursor now points past the retained window, so
    // the first read reports the gap rather than silently skipping.
    match rx.try_recv() {
        Err(TryRecvError::Lagged(n)) => {
            assert!(n > 0, "expected a positive lag count");
        }
        other => panic!("expected Lagged after overflow, got {other:?}"),
    }

    // The snapshot the bridge would forward reflects ground truth —
    // every project, not just the buffered tail.
    let snapshot = ws.snapshot();
    assert_eq!(snapshot.len(), total);

    match ws.resync_event() {
        WorkspaceEvent::Resync(projects) => {
            assert_eq!(projects.len(), total);
            // Snapshot is position-sorted; first created lands first.
            assert_eq!(projects[0].name, "p0");
        }
        other => panic!("expected Resync event, got {other:?}"),
    }
}
