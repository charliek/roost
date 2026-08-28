//! Headless per-tab PTY drains.
//!
//! A UI drains a tab's output because it has a terminal to write the
//! bytes into. A session has neither, but it still has to consume the
//! stream: the workspace facts a tab reports — its title, its cwd, its
//! prompt marks, its notifications — arrive as OSC sequences *inside*
//! that stream, and nobody else is scanning it. So each tab gets a
//! drain that reads the bytes, keeps the OSC actions, and throws the
//! bytes away.
//!
//! # Scaffolding, deliberately local
//!
//! This lives in the session crate rather than on `roost-engine`'s
//! public surface because it is temporary. HS-1b gives a session a real
//! terminal per tab (that is what makes `attach` possible at all), and
//! at that point the drain becomes "feed the terminal" and this module
//! goes away. Promoting it to the engine now would make a placeholder
//! look like an API.
//!
//! # What a headless drain cannot do
//!
//! * **Client-local OSC actions are dropped.** Clipboard writes and
//!   pointer-shape changes are things a *view* does; there is no view.
//!   They are logged at debug and discarded rather than queued, because
//!   a client attaching later wants the clipboard as it is then, not a
//!   replay of every write since the session started.
//! * **Terminal-generated queries go unanswered in HS-1a.** A program
//!   that asks the terminal who it is (DA/DA2), where the cursor is
//!   (DSR), or what a palette entry is set to gets no reply, because
//!   answering requires a libghostty terminal and this slice has none.
//!   Programs that block on such a reply will hang; those that time out
//!   degrade. HS-1b's per-tab terminal is what fixes it.
//!
//! # The fast-exit race, and why there is an exit ledger
//!
//! A workspace row is opened *before* its PTY is spawned (the dispatcher
//! rolls the row back if the spawn fails), so a drain that starts on
//! `TabOpened` has to wait for the PTY to appear. Meanwhile the
//! supervisor's reap task, on the way out, **removes the session before
//! it publishes anything about the exit** (`pty.rs`: remove, then
//! `TabExited`, then `Exit`) — deliberately, so nobody can find a live
//! session for a dead child.
//!
//! Put together: for a command that exits in the time it takes this task
//! to be scheduled — `sh -c 'exit 0'` is enough — a poll of the
//! supervisor finds nothing, and "not spawned yet" and "already gone"
//! look identical. Waiting produces a phantom row that outlives its
//! process, gets persisted at stop, and comes back on the next start.
//!
//! `TabExited` is the signal that tells the two apart, and it is only
//! useful if it cannot be missed. So the attacher subscribes to the
//! supervisor's lifecycle channel **once, before the first tab can be
//! opened**, and records every exit in a small ledger the per-tab attach
//! loop checks. See [`spawn_attacher`] for the ordering argument.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use roost_engine::osc::{OscAction, OscColorSnapshot, OscRgb};
use roost_engine::session::{TabOutput, TabSession};
use roost_engine::{LocalClient, PtySupervisor, SupervisorEvent, Workspace, WorkspaceEvent};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, info, warn};

use crate::consts::{ATTACH_POLL_INTERVAL, ATTACH_TIMEOUT};

/// Tabs the supervisor has reported reaped, held only until the tab's
/// own drain notices.
///
/// Ids are never reused within a session (the workspace mints them from
/// a monotonic counter), so an entry can only ever be claimed by the tab
/// that produced it.
#[derive(Default)]
struct ExitLedger(Mutex<HashSet<i64>>);

impl ExitLedger {
    fn record(&self, tab_id: i64) {
        self.lock().insert(tab_id);
    }

    /// Claim `tab_id`'s exit if one was recorded, clearing it either way
    /// so the ledger stays the size of the tabs currently dying rather
    /// than of every tab the session has ever run.
    fn take(&self, tab_id: i64) -> bool {
        self.lock().remove(&tab_id)
    }

    /// A poisoned lock means a panic elsewhere; losing exit tracking
    /// would leak rows for the rest of the session, which is strictly
    /// worse than continuing with the set we have.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashSet<i64>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Watch the workspace and give every tab that opens a headless drain.
///
/// # Ordering, which is the whole design
///
/// Two subscriptions, both taken here and both taken *before* the caller
/// hydrates:
///
/// * `events` is passed in already subscribed, so the restored tabs'
///   `TabOpened` events are queued before this task runs. Both origins
///   land here — hydration and `tab.open` over IPC both go through
///   `Workspace::open_tab` — which is the point: one attach path, and it
///   is not the dispatcher's business.
/// * the supervisor's lifecycle channel is subscribed on the line below,
///   which `serve` reaches before hydration and therefore before any
///   `spawn` has happened. A `broadcast` receiver only sees messages sent
///   after it subscribed, so this placement is what makes "no `TabExited`
///   can predate the subscription" true rather than likely. It must not
///   move into `drain_tab`: that task starts *after* its tab's spawn, so
///   a subscription there could be created after the exit it needs.
///
/// The loop then selects over both, so an exit is recorded promptly even
/// while a burst of opens is being serviced. Both `recv`s are
/// cancel-safe, so the select cannot drop an event it did not return.
pub fn spawn_attacher(
    client: LocalClient,
    mut events: broadcast::Receiver<WorkspaceEvent>,
) -> tokio::task::JoinHandle<()> {
    let mut lifecycle = client.supervisor.subscribe_lifecycle();
    let exits = Arc::new(ExitLedger::default());
    tokio::spawn(async move {
        loop {
            tokio::select! {
                event = events.recv() => match event {
                    Ok(WorkspaceEvent::TabOpened(tab)) => {
                        tokio::spawn(drain_tab(client.clone(), tab.id, Arc::clone(&exits)));
                    }
                    Ok(_) => {}
                    // A session that fell behind its own workspace events
                    // has missed tab opens, and a tab with no drain never
                    // reports its title or closes on exit. Nothing here can
                    // recover the lost ids; surface it loudly.
                    Err(RecvError::Lagged(dropped)) => {
                        warn!(
                            dropped,
                            "session workspace events lagged; tabs may be undrained"
                        );
                    }
                    Err(RecvError::Closed) => break,
                },
                event = lifecycle.recv() => match event {
                    Ok(SupervisorEvent::TabExited { tab_id, .. }) => exits.record(tab_id),
                    // Dropped exits fall back to the attach deadline,
                    // which still closes the row — just slowly.
                    Err(RecvError::Lagged(dropped)) => {
                        warn!(dropped, "session pty lifecycle events lagged");
                    }
                    // The sender lives in the supervisor this task holds
                    // through `client`, so this is unreachable; if it
                    // ever fires the session is over anyway.
                    Err(RecvError::Closed) => break,
                },
            }
        }
    })
}

/// Attach to one tab and consume its output until the PTY is gone.
async fn drain_tab(client: LocalClient, tab_id: i64, exits: Arc<ExitLedger>) {
    match attach(&client.supervisor, &client.workspace, tab_id, &exits).await {
        Attach::Ready(receiver) => consume(&client, tab_id, receiver).await,
        Attach::Gone => {}
        // The row was rolled back (a failed spawn) or closed by a
        // client. Nothing to drain and nothing to clean up.
        Attach::RowClosed => return,
    }

    // One exit point for every way a drain can end — a published `Exit`,
    // an output channel that closed without one, or an exit learned
    // before this task ever attached. The PTY is gone in all three, and
    // the row must not outlive it.
    exits.take(tab_id);
    if let Err(error) = client.workspace.close_tab(tab_id) {
        debug!(tab_id, %error, "tab row was already gone at PTY exit");
    }
}

/// Feed the tab's output through the OSC scan until the stream ends.
async fn consume(
    client: &LocalClient,
    tab_id: i64,
    receiver: broadcast::Receiver<roost_engine::PtyOutputEvent>,
) {
    let (output_tx, mut output_rx) = tokio::sync::mpsc::unbounded_channel();
    // `session` is bound for the whole drain: it owns the tab's serial
    // command channel, and dropping it would end the tab's PTY writer.
    let _session = TabSession::attach_with_receiver_scanned(
        Arc::clone(&client.supervisor),
        tab_id,
        receiver,
        output_tx,
        None,
        Some(headless_color_seed()),
    );

    while let Some(output) = output_rx.recv().await {
        match output {
            // The scan opt-in is on, so every real chunk arrives as
            // `Scanned`. `Bytes` is the un-opted-in shape and cannot
            // occur here; treat it as a chunk with no actions rather
            // than asserting in a drain.
            TabOutput::Bytes(_) => {}
            TabOutput::Scanned { actions, .. } => {
                for action in actions {
                    apply(client, tab_id, action);
                }
            }
            TabOutput::Exit { status, reason } => {
                info!(tab_id, status, %reason, "session PTY exited");
                break;
            }
            TabOutput::Error(error) => {
                // Dropped bytes cannot be reconstructed, and the OSC
                // scanner's streaming state may now be mid-sequence.
                // Keep draining: the alternative is a live tab whose
                // title and cwd silently stop updating for good.
                warn!(tab_id, %error, "session PTY output stream lost bytes");
            }
        }
    }
}

/// What the wait for a tab's PTY settled on.
enum Attach {
    /// The PTY is live; here is its output.
    Ready(broadcast::Receiver<roost_engine::PtyOutputEvent>),
    /// There is no PTY and there never will be — it exited before this
    /// drain could attach. The row is stale and must be closed.
    Gone,
    /// The workspace no longer lists the row, so there is nothing left
    /// to reconcile.
    RowClosed,
}

/// Wait for the tab's PTY, then take the receiver the supervisor stashed
/// before its reader task started.
///
/// The wait exists because `TabOpened` is published before the spawn:
/// the dispatcher opens the workspace row first so it can roll it back
/// if the spawn fails. Taking the *stashed* receiver rather than a fresh
/// subscription is what keeps a fast command's first bytes — the ones
/// emitted before this task ran at all.
async fn attach(
    supervisor: &Arc<PtySupervisor>,
    workspace: &Arc<Workspace>,
    tab_id: i64,
    exits: &ExitLedger,
) -> Attach {
    let deadline = tokio::time::Instant::now() + ATTACH_TIMEOUT;
    loop {
        // Checked before the probe, and every iteration: the reap task
        // removes the session *before* it announces the exit, so a probe
        // that finds nothing cannot tell "not spawned yet" from "already
        // reaped". This ledger is the only thing that can, and its
        // subscription predates every spawn (see `spawn_attacher`).
        if exits.take(tab_id) {
            debug!(tab_id, "the PTY exited before this drain could attach");
            return Attach::Gone;
        }
        if let Some(receiver) = supervisor
            .take_initial_receiver(tab_id)
            .or_else(|| supervisor.subscribe_output(tab_id))
        {
            return Attach::Ready(receiver);
        }
        // A row that is no longer listed lost its spawn and was rolled
        // back, so there will never be a PTY to attach to.
        if !workspace
            .snapshot()
            .iter()
            .any(|project| project.tabs.iter().any(|tab| tab.id == tab_id))
        {
            debug!(tab_id, "tab vanished before its PTY appeared");
            return Attach::RowClosed;
        }
        if tokio::time::Instant::now() >= deadline {
            // Only reachable if the lifecycle broadcast lagged and lost
            // this tab's `TabExited`. Closing the row is still the right
            // answer — a listed tab with no process is a phantom — so
            // this is the backstop, not a second failure mode.
            warn!(
                tab_id,
                "no PTY appeared within the attach deadline; closing the row"
            );
            return Attach::Gone;
        }
        tokio::time::sleep(ATTACH_POLL_INTERVAL).await;
    }
}

/// Route one scanned action. Workspace-directed actions are applied
/// through the same `LocalClient::apply_osc` a UI uses, so a session and
/// a UI derive identical state from identical bytes.
fn apply(client: &LocalClient, tab_id: i64, action: OscAction) {
    match action {
        OscAction::Workspace { command, payload } => client.apply_osc(tab_id, command, &payload),
        OscAction::ClipboardWrite { target, .. } => {
            debug!(
                tab_id,
                ?target,
                "dropped an OSC clipboard write: no client attached"
            );
        }
        OscAction::PointerShape(shape) => {
            debug!(tab_id, %shape, "dropped an OSC pointer shape: no client attached");
        }
    }
}

/// The theme seed the drain's color tracker starts from.
///
/// A UI seeds this with its real palette because it answers color
/// queries from it. This session answers none (see the module docs), so
/// the values are never read back — but the scanner's opt-in is what
/// produces OSC actions at all, and it is gated on having a seed. Plain
/// black-on-white with a zeroed palette states "no theme" honestly
/// rather than inventing one a client might later be told is real.
fn headless_color_seed() -> OscColorSnapshot {
    const BLACK: OscRgb = (0, 0, 0);
    const WHITE: OscRgb = (0xff, 0xff, 0xff);
    OscColorSnapshot::new(WHITE, BLACK, WHITE, [BLACK; 256])
}
