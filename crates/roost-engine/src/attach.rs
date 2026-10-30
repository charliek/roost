//! The attach data plane: one forwarder per admitted data connection
//! (plan 036 D6; `discovery/host-sessions-architecture.md` §4.3).
//!
//! A data connection arrives as a JSON handshake and, if it is
//! admissible, turns binary for the rest of its life. What flows over it
//! is one tab's terminal, in two overlapping streams:
//!
//! ```text
//!   snapshot bytes ──► SNAP frames  ─┐
//!                                    ├─► the client's decoder
//!   tab tee (seq'd) ──► PTY frames  ─┘
//!                       EXIT frame  ── always last
//! ```
//!
//! The two are scheduled against each other frame by frame. Live PTY
//! traffic leads — a client typing into a tab must not wait out a
//! 2000-line scrollback — but the snapshot is guaranteed progress
//! ([`PTY_BURST_BYTES`] of PTY payload or [`SNAP_PROGRESS_INTERVAL`],
//! whichever comes first), because a `yes`-style producer would
//! otherwise starve FINISH for as long as it kept running. Everything
//! through the READY record goes at full speed: that prefix is what the
//! client renders from, and the history behind it is catch-up.
//!
//! How a client knows the snapshot half is over depends on the
//! negotiated payload kind. GHOSTSNP says so inside its own framing (a
//! FINISH record), so the pump sends nothing extra. A `vt` payload is a
//! bare byte stream with no internal marks at all, so the pump ends it
//! with **one zero-length SNAP frame** — the meaning `dataframe.rs`
//! reserved for the empty frame, and the reason the whole payload, not
//! a prefix of it, is what PTY traffic is held behind there.
//!
//! # The fence, and why gaps are fatal
//!
//! The forwarder subscribes to the tab's tee **before** it asks for the
//! snapshot, so no byte can fall between the encode and the
//! subscription. The snapshot's `seq` is the fence `S`: everything at or
//! below it is already inside the snapshot and is discarded, and from
//! there the stream must be exactly `S+1, S+2, …`. A gap, a duplicate,
//! or a lagged tee is a **server** bug, not something to paper over —
//! the client's terminal would silently diverge — so it ends the
//! connection with `ERROR desync` and lets a re-attach rebuild from a
//! fresh snapshot.
//!
//! # Resume
//!
//! A client that already holds this tab's stream up to some seq can ask
//! for the rest instead of a whole new snapshot, and then the shape
//! above loses its left half:
//!
//! ```text
//!   ring slice ──┐
//!                ├─► PTY frames ─► the client's decoder
//!   tab tee ─────┘
//! ```
//!
//! The fence is `resume_from_seq - 1`, there are no SNAP frames at all,
//! and the ring records the client missed are framed through the same
//! contiguity walk the live ones are — a hole in the ring is as fatal as
//! a hole in the tee. The slice and the tee subscription come out of the
//! tab task together ([`TabCmd::Resume`]), so nothing can be published
//! between reading one and taking the other.
//!
//! Resume is an optimization, never a demand: a stale epoch, a respawned
//! tab, a seq the ring no longer covers, a grid change since that seq, or
//! a tab task that went away all fall back to a full snapshot **in the
//! same reply**. A client is told which it got by `mode`, and never has
//! to handle a resume failure.
//!
//! # Size ownership (plan 084 D4)
//!
//! Every attachment is a [`TabCmd::Attach`] decision on the tab task —
//! sent on the admitted generation's channel, after a [`DetachGuard`] is
//! armed and after the client's reader has started — and ends in exactly
//! one [`TabCmd::Detach`], whatever ends it. Until its accepted reply is
//! written (`AttachCommitted`) an attachment is a cancel waiting to
//! happen: an EOF or a `DETACH` read during the decision, the fence or
//! the hold rolls back whatever it took, and the frames it sent meanwhile
//! wait for the commit.
//!
//! `GRID` rides the tee beside the PTY records but outside their seq
//! rule: it is held with them until READY, counted against the same
//! queue, and dropped when the snapshot or resume already reported it.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use roost_ipc::dataframe::{
    write_data_frame, write_preamble, DataFrame, DataFrameReader, InputTake, ResizePayload,
    FRAME_DETACH, FRAME_ERROR, FRAME_EXIT, FRAME_GRID, FRAME_INPUT, FRAME_PTY, FRAME_RESIZE,
    FRAME_SNAP, FRAME_TAKE, MAX_DATA_FRAME_BYTES,
};
use roost_ipc::framing::write_frame;
use roost_ipc::messages::{
    AttachAccepted, AttachHandshake, AttachHandshakeReply, AttachMode, AttachPayloadKind,
    GridFrame, ResponseError, SizeOwner,
};
use roost_ipc::{codes, CloseReason, ConnCloseWatch, ConnCtx, DataConn, CLOSE_LABEL_DEADLINE};
use tokio::io::AsyncWriteExt;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tracing::{debug, warn};

use crate::ipc::{attach_resize_refusal, AdmittedAttach, IpcHandler};
use crate::pty::{Geometry, PtyOutputEvent};
use crate::size_owner::AttachmentId;
use crate::tab_task::{AttachDecision, SnapshotAt, TabCmd, TabError};

/// PTY payload that may go out between two SNAP frames before the
/// snapshot is guaranteed a turn.
pub const PTY_BURST_BYTES: usize = 256 * 1024;
/// Longest a SNAP frame may be held back by PTY traffic. The floor that
/// makes FINISH arrive under a slow-but-endless producer, where the byte
/// burst alone never trips.
pub const SNAP_PROGRESS_INTERVAL: Duration = Duration::from_millis(50);
/// Cap on PTY bytes read off the tee but not yet written. Past it the
/// peer is not reading and the connection ends rather than growing.
pub const FORWARDER_QUEUE_BYTES: usize = 8 * 1024 * 1024;
/// Cap on one SNAP frame of a `vt` payload — deliberately far below
/// [`MAX_DATA_FRAME_BYTES`], and for `vt` only.
///
/// The pump drains the tab's tee *between* writes, never during one, and
/// a `vt` payload holds every PTY frame behind it (its ready prefix is
/// the whole payload, not a marked prefix of it). So one write is a
/// window in which the tee has to absorb everything the child produces:
/// past the output broadcast's depth — 256 chunks of at most 4 KiB,
/// about a megabyte — the subscription lags, and a lag ends the
/// attach as `desync`, not as the `overflow` that
/// [`FORWARDER_QUEUE_BYTES`] reports and that a re-attach recovers from
/// cleanly. At 64 KiB a child has to outrun the client's link by ~16x
/// *within a single write* to reach that; anything slower accumulates in
/// the held batch instead, where the queue cap bounds it and names the
/// failure correctly. The cost is 16 writes per megabyte instead of one.
///
/// GHOSTSNP keeps the full frame: its hold ends at READY, which is a
/// prefix of the payload and not the whole of it.
pub const VT_SNAP_FRAME_BYTES: usize = 64 * 1024;
/// Cap on what one attach carries before its snapshot finishes: the
/// snapshot payload plus the live PTY payload written alongside it.
/// Counting only the snapshot would let an endless producer ride an
/// unfinished attach for as many bytes as it liked.
pub const ATTACH_BYTE_BUDGET: usize = 512 * 1024 * 1024;
/// Cap on how long the snapshot half of an attach may take, measured
/// from the fence — the encode queues behind
/// [`crate::tab_task::MAX_CONCURRENT_SNAPSHOTS`], and that wait is part
/// of what a client is sitting through.
pub const ATTACH_TIME_BUDGET: Duration = Duration::from_secs(60);
/// How long one write may sit unfinished before the peer counts as
/// stalled. Same reasoning (and value) as a push connection's deadline:
/// a healthy client drains a frame in microseconds.
const WRITE_DEADLINE: Duration = roost_ipc::DEFAULT_PUSH_WRITE_DEADLINE;
/// Depth of the client-frame channel between the reader task and the
/// forwarder. Bounded: a client that floods INPUT faster than the tab
/// can take it should feel backpressure on its own socket.
const CLIENT_FRAME_CAPACITY: usize = 64;

/// Answer a data connection this socket cannot serve, then close.
pub(crate) async fn refuse(conn: DataConn, code: &str, message: &str) {
    let (_reader, mut writer, _close) = conn.into_parts();
    reject(&mut writer, code, message).await;
}

/// Serve one data connection: admit it, decide its size, fence it, and
/// stream its tab until something ends it.
pub(crate) async fn serve_attach(
    h: &IpcHandler,
    ctx: &ConnCtx,
    handshake: AttachHandshake,
    conn: DataConn,
) {
    let (reader, mut writer, close) = conn.into_parts();

    // A connection the server has already closed gets nothing — not even
    // a rejection. The closer firing IS the answer, and writing after it
    // would be this socket disagreeing with itself about whether the
    // client is still admitted.
    if close.reason().is_some() {
        return;
    }

    // The generation check already ran on the raw line, in the IPC
    // server, so nothing here can be reached by a client that disagrees
    // about what these fields mean.
    let admitted = match h.admit_attach(&handshake, ctx) {
        Ok(admitted) => admitted,
        Err(error) => {
            reject(&mut writer, &error.code, &error.message).await;
            return;
        }
    };

    // Set by the reader the moment it reads `DETACH`, so however this
    // attachment ends after that, it ends as a deliberate detach.
    let detached = Arc::new(AtomicBool::new(false));
    // Read from the start, so a client that leaves or detaches while its
    // attach is still being decided cancels it instead of committing it.
    let (mut client, reading) = ClientFrames::start(reader, Arc::clone(&detached));
    // Armed before anything reaches the tab task, so every way out of
    // here from now on — a rejection, a cancel, a fault, an unwind —
    // tells the tab this attachment is gone, exactly once.
    let mut guard = DetachGuard {
        h,
        detach: Some(Detach {
            tab_id: admitted.tab_id,
            id: ctx.conn_id,
            commands: admitted.commands.clone(),
            detached,
            reader: Some(reading),
        }),
    };

    // Test seam only (`AttachPause`), and a no-op without one: the
    // window between the admission that captured this tab's identity
    // and the first thing done with it, which a test stands inside to
    // respawn the tab there.
    h.supervisor.pause_attach_admission().await;
    attach_tab(
        h,
        &admitted,
        &handshake,
        &mut client,
        writer,
        close,
        ctx.conn_id,
    )
    .await;
    // The ordinary end waits the reader out here, so its `Detach` goes
    // out at once, ahead of the registry release.
    guard.reader_stopped().await;
}

/// Decide, fence, answer the handshake, commit, and pump until the end.
async fn attach_tab(
    h: &IpcHandler,
    admitted: &AdmittedAttach,
    handshake: &AttachHandshake,
    client: &mut ClientFrames,
    mut writer: OwnedWriteHalf,
    close: ConnCloseWatch,
    id: AttachmentId,
) {
    let tab_id = admitted.tab_id;
    let tab_generation = admitted.tab_generation;
    let kind = &admitted.kind;
    // Started before the snapshot is asked for, not after it arrives:
    // the encode queues behind `MAX_CONCURRENT_SNAPSHOTS` and is exactly
    // the part of an attach the time budget exists to bound.
    let started = Instant::now();

    // The size decision (rule 2): a take resizes the tab before anything
    // is encoded from it. It runs on the forwarder's own task and under
    // no session-state lock, because a tab parked behind
    // `MAX_CONCURRENT_SNAPSHOTS` must not stall every other session op.
    let decision = match client.during(decide_attach(admitted, id)).await {
        Ok(Some(decision)) => decision,
        Ok(None) => {
            reject(
                &mut writer,
                codes::NOT_FOUND,
                &format!("tab {tab_id} is gone"),
            )
            .await;
            return;
        }
        Err(cancel) => return cancel.answer(&mut writer).await,
    };
    // The child half of a take, waited out here and never in the tab
    // task: the snapshot below must not be encoded before the child was
    // told, and a wave of attaches must not stall the tab behind it.
    match client.during(decision.settled(tab_id)).await {
        Ok(Ok(_provisional)) => {}
        Ok(Err(error)) => {
            let refused = attach_resize_refusal(
                tab_id,
                admitted.geometry.cols,
                admitted.geometry.rows,
                &error,
            );
            reject(&mut writer, &refused.code, &refused.message).await;
            return;
        }
        Err(cancel) => return cancel.answer(&mut writer).await,
    }

    // Resume first, and only ever as an optimization: everything it
    // cannot honor comes back `None` and is served as a full attach
    // under the same reply (D6).
    let resumed = match client
        .during(resume_tab(h, tab_generation, tab_id, handshake))
        .await
    {
        Ok(resumed) => resumed,
        Err(cancel) => return cancel.answer(&mut writer).await,
    };
    let attached = match resumed {
        Some(attached) => attached,
        None => match client
            .during(fence_tab(h, tab_generation, tab_id, kind))
            .await
        {
            Ok(Ok(fenced)) => fenced,
            Ok(Err((code, message))) => {
                reject(&mut writer, code, &message).await;
                return;
            }
            Err(cancel) => return cancel.answer(&mut writer).await,
        },
    };

    if let Some(hold) = h.supervisor.attach_hold() {
        if let Err(cancel) = client.during(tokio::time::sleep(hold)).await {
            return cancel.answer(&mut writer).await;
        }
    }

    let Attached {
        tee,
        commands,
        mode,
        seq: fence,
        server_epoch,
        tab_generation: live_generation,
        snapshot,
        reported_size,
        owner,
        grid_gen,
        ready_end,
        terminator,
        snap_frame_bytes,
        replay,
        stored_exit,
    } = attached;

    // The fence is a round trip through the tab task, and a stop during
    // it means there is no session left to stream from. Answering
    // `accepted` now would hand this client a stream nothing will ever
    // write to.
    if close.reason().is_some() {
        debug!(
            tab_id,
            "the data connection closed while its tab was fenced"
        );
        return;
    }

    // Reported on every accepted reply, snapshot or resume, taking or
    // not: another client may take the tab between this attach's own
    // decision and the encode, and this is the only place that says what
    // the bytes to follow were actually written for — and by whose size.
    // Sending it costs a client that already agrees nothing.
    let accepted = AttachHandshakeReply::Accepted(AttachAccepted {
        // Restated rather than assumed: the handshake states a
        // preference order, so the reply is the one place the client
        // hears which entry it got.
        kind: kind.clone(),
        mode,
        seq: fence,
        server_epoch,
        tab_generation: live_generation,
        snapshot_cols: reported_size.0,
        snapshot_rows: reported_size.1,
        owner,
    });
    let Ok(body) = serde_json::to_vec(&accepted) else {
        return;
    };
    // The last look before the reply goes out: whatever the client has
    // already sent that ends this attach ends it uncommitted.
    if let Err(cancel) = client.drain().await {
        return cancel.answer(&mut writer).await;
    }
    if write_frame(&mut writer, &body).await.is_err() {
        return;
    }
    if write_preamble(&mut writer).await.is_err() {
        return;
    }
    // Committed only now: a client that is still deciding whether to
    // believe this reply is not yet a candidate for anything.
    if commands.send(TabCmd::AttachCommitted { id }).await.is_err() {
        debug!(tab_id, "the tab task went away before the attach committed");
    }

    let ending = Pump {
        tab_id,
        id,
        commands,
        tee,
        writer,
        close,
        dropped: admitted.dropped.clone(),
        snapshot,
        ready_end,
        terminator,
        snap_frame_bytes,
        replay,
        stored_exit,
        fence,
        grid_gen,
        started,
    }
    .run(client)
    .await;
    debug!(tab_id, ?mode, ?ending, "attach data connection ended");
}

/// Put this attachment to the tab task and wait for its decision.
/// `None` when the task is gone.
async fn decide_attach(admitted: &AdmittedAttach, id: AttachmentId) -> Option<AttachDecision> {
    let (reply, decided) = oneshot::channel();
    admitted
        .commands
        .send(TabCmd::Attach {
            id,
            client_id: admitted.client_id.clone(),
            label: admitted.label.clone(),
            take: admitted.take,
            geometry: admitted.geometry,
            reply,
        })
        .await
        .ok()?;
    decided.await.ok()
}

/// The one `Detach` every attachment ends in, and the registry entry it
/// gives back.
///
/// Sent from `Drop`, so a rejection, a cancel, a fault, the pump's end and
/// an unwind all reach it. A `Detach` the tab task has no attachment for
/// is a no-op, so one that outruns an `Attach` that was never sent is
/// harmless — and one cannot outrun an `Attach` that was, because that
/// send is awaited before anything can drop this.
struct DetachGuard<'a> {
    h: &'a IpcHandler,
    detach: Option<Detach>,
}

impl DetachGuard<'_> {
    async fn reader_stopped(&mut self) {
        if let Some(detach) = self.detach.as_mut() {
            detach.stop_reader().await;
        }
    }
}

impl Drop for DetachGuard<'_> {
    fn drop(&mut self) {
        let Some(detach) = self.detach.take() else {
            return;
        };
        let (tab_id, id) = (detach.tab_id, detach.id);
        detach.send();
        // Registered by the admission; given back here however the
        // attachment ended, so a tab that is attached and detached
        // repeatedly does not accumulate entries.
        self.h.release_data_conn(tab_id, id);
    }
}

/// The `Detach` itself. Whether it is deliberate is settled only once the
/// client's reader has stopped — a reader that has decoded a `DETACH` may
/// not have recorded it yet, and an abort cannot reach it there — so one
/// sent while the reader still runs waits for it on a task of its own.
struct Detach {
    tab_id: i64,
    id: AttachmentId,
    commands: mpsc::Sender<TabCmd>,
    /// The client sent `DETACH` — before its commit or after.
    detached: Arc<AtomicBool>,
    reader: Option<tokio::task::JoinHandle<()>>,
}

impl Detach {
    /// Awaited through the handle in place, so a caller cancelled
    /// mid-wait leaves it for [`Self::send`] to wait out.
    async fn stop_reader(&mut self) {
        if let Some(reader) = self.reader.as_mut() {
            reader.abort();
            let _ = reader.await;
            self.reader = None;
        }
    }

    fn send(mut self) {
        let running = self.reader.take().and_then(|reader| {
            reader.abort();
            (!reader.is_finished()).then_some(reader)
        });
        let Some(reader) = running else {
            return self.send_now();
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    let _ = reader.await;
                    let detach = self.command();
                    let _ = self.commands.send(detach).await;
                });
            }
            Err(_) => {
                warn!(
                    tab_id = self.tab_id,
                    "a detach could not wait for its reader with no runtime to wait on"
                );
                self.send_now();
            }
        }
    }

    /// `try_send` first; a full channel gets a task of its own rather than
    /// a dropped detach, which would leave the tab owned by a connection
    /// that no longer exists.
    fn send_now(self) {
        match self.commands.try_send(self.command()) {
            Ok(()) | Err(TrySendError::Closed(_)) => {}
            Err(TrySendError::Full(detach)) => match tokio::runtime::Handle::try_current() {
                Ok(runtime) => {
                    runtime.spawn(async move {
                        let _ = self.commands.send(detach).await;
                    });
                }
                Err(_) => warn!(
                    tab_id = self.tab_id,
                    "a detach found the tab's channel full with no runtime to wait on it"
                ),
            },
        }
    }

    fn command(&self) -> TabCmd {
        TabCmd::Detach {
            id: self.id,
            deliberate: self.detached.load(Ordering::Acquire),
        }
    }
}

/// A tab pinned for one attach: the live tee, the fence every byte that
/// follows is measured against, and whichever catch-up the client is
/// owed — an encoded snapshot (`mode: "snapshot"`) or the ring records it
/// missed (`mode: "resume"`), never both.
struct Attached {
    tee: broadcast::Receiver<PtyOutputEvent>,
    commands: mpsc::Sender<TabCmd>,
    mode: AttachMode,
    /// The fence: the client has everything up to and including this,
    /// and the first PTY frame carries `seq + 1`.
    seq: u64,
    server_epoch: u64,
    tab_generation: u64,
    /// Empty in resume mode — the client already has this history.
    snapshot: Vec<u8>,
    /// The grid the bytes that follow were written for, read on the tab
    /// task: the snapshot's encode geometry, or in resume mode the
    /// tab's own at the handoff. Always known, which is why it is not an
    /// `Option` the way the wire fields are — those are absent only from
    /// a session that predates them.
    reported_size: (u16, u16),
    /// The size owner in that same turn, and the generation every `GRID`
    /// at or below which the reply already covers.
    owner: Option<SizeOwner>,
    grid_gen: u64,
    ready_end: usize,
    /// Whether a zero-length `SNAP` frame ends the snapshot half.
    ///
    /// `vt`'s payload is a byte stream with no internal marker, so the
    /// end of it is the only thing that can tell a client its terminal
    /// is complete; GHOSTSNP carries FINISH and sends no empty frame.
    /// False on a resume, which has no snapshot half at all.
    terminator: bool,
    /// Cap on one SNAP frame — see [`VT_SNAP_FRAME_BYTES`].
    snap_frame_bytes: usize,
    /// Pre-fence records from the replay ring, sent as ordinary PTY
    /// frames ahead of the live tee. Empty in snapshot mode.
    replay: Vec<(u64, Vec<u8>)>,
    /// An exit the tab published before this connection subscribed, so a
    /// resume that arrives after the tab died still ends in EXIT.
    stored_exit: Option<(u64, i32)>,
}

/// Everything that must hold before a byte of terminal goes out, in the
/// order it has to be checked in (D5): each failure tells the client to
/// fix a different thing, so an earlier one must not be masked by a
/// later one. `Err` carries the rejection the client is owed.
async fn fence_tab(
    h: &IpcHandler,
    tab_generation: u64,
    tab_id: i64,
    kind: &AttachPayloadKind,
) -> Result<Attached, (&'static str, String)> {
    let no_terminal = || {
        (
            codes::NOT_FOUND,
            "that tab has no live terminal".to_string(),
        )
    };
    // Subscribed FIRST, before the snapshot is even asked for: the
    // encode runs on the tab task between two chunks, so a subscription
    // taken afterwards could miss whatever landed in between. Discarding
    // pre-fence events is cheap; recovering a lost one is impossible.
    let tee = h
        .supervisor
        .subscribe_output(tab_id)
        .ok_or_else(no_terminal)?;
    // Channel and generation in one lock acquisition, so the channel
    // this snapshot is asked on belongs to the generation checked here.
    // A respawn between the subscribe above and this read gives a new
    // generation, which the check below turns into a clean `not-found`
    // rather than a tee and a snapshot from two different terminals. A
    // respawn *after* it is settled by [`generation_holds`], on what the
    // task reports having served.
    let (commands, live_generation) = h
        .supervisor
        .tab_task_handle(tab_id)
        .ok_or_else(no_terminal)?;
    if live_generation != tab_generation {
        return Err((
            codes::NOT_FOUND,
            "that tab was respawned after this attach was admitted".to_string(),
        ));
    }

    let snapshot = take_snapshot(&commands, kind)
        .await
        .map_err(|error| match error {
            // Re-attach is the recovery: the failure is about the terminal's
            // state at this instant, not about the client.
            TabError::SnapshotFailed(why) => (codes::SNAPSHOT_FAILED, why),
            other => (codes::NOT_FOUND, other.to_string()),
        })?;

    // The attach named a tab pipeline, not just a tab id: a respawn is a
    // different terminal with a different seq space, and streaming it
    // under the old identity is exactly what the generation exists to
    // prevent.
    if !generation_holds(h, tab_id, snapshot.tab_generation, tab_generation) {
        return Err((
            codes::NOT_FOUND,
            "that tab was respawned while its snapshot was being taken".to_string(),
        ));
    }

    // What the client renders from, held ahead of every live PTY frame.
    // GHOSTSNP marks it with a READY record partway through; a `vt`
    // stream has no such marker, so its ready prefix is the whole
    // payload — a client cannot draw a partially replayed screen.
    let is_vt = kind.as_str() == AttachPayloadKind::VT;
    let ready_end = if is_vt {
        snapshot.bytes.len()
    } else {
        roost_vt::ready_boundary(&snapshot.bytes).map_err(|error| {
            (
                codes::SNAPSHOT_FAILED,
                format!("the encoded snapshot has no READY record: {error}"),
            )
        })?
    };

    Ok(Attached {
        tee,
        commands,
        mode: AttachMode::Snapshot,
        seq: snapshot.seq,
        server_epoch: snapshot.server_epoch,
        tab_generation: snapshot.tab_generation,
        reported_size: (snapshot.cols, snapshot.rows),
        owner: snapshot.owner,
        grid_gen: snapshot.grid_gen,
        snapshot: snapshot.bytes,
        ready_end,
        terminator: is_vt,
        snap_frame_bytes: if is_vt {
            VT_SNAP_FRAME_BYTES
        } else {
            MAX_DATA_FRAME_BYTES
        },
        replay: Vec::new(),
        stored_exit: None,
    })
}

/// Whether the pipeline that just served a hand-off is still the one
/// this attach was admitted for.
///
/// `served` is reported by the tab task itself, in the turn it cut the
/// hand-off: which task answered is a **fact** there, where the lookup
/// before the send is only an inference about which one was going to.
/// `admitted` is the generation the attach was let in for. The
/// supervisor's current generation closes the other half of the window:
/// a respawn that lands while the command is queued leaves the old task
/// serving a terminal this tab id no longer names, and the client would
/// be handed the dead terminal's backlog and its EXIT instead of the
/// replacement.
///
/// A tab with **no** pipeline at all is not a mismatch. It exited, and a
/// stream that ends in EXIT is the truth about the terminal this client
/// asked for.
fn generation_holds(h: &IpcHandler, tab_id: i64, served: u64, admitted: u64) -> bool {
    served == admitted
        && h.supervisor
            .tab_generation(tab_id)
            .is_none_or(|live| live == served)
}

/// The resume handoff (D6), or `None` for every reason it cannot be
/// honored — a stale epoch, a respawned tab, a seq outside the ring, a
/// grid change inside the slice, a tab task that is gone. Never an
/// error: the caller serves a full snapshot instead, in the same reply,
/// and a client that has been away too long simply pays for a snapshot
/// rather than being told off.
///
/// Nothing here subscribes to the tee. The whole point is that the ring
/// slice and the subscription come back from one turn of the tab task
/// ([`TabCmd::Resume`]), with no instant in between for a record to fall
/// into.
async fn resume_tab(
    h: &IpcHandler,
    tab_generation: u64,
    tab_id: i64,
    handshake: &AttachHandshake,
) -> Option<Attached> {
    let from_seq = handshake.resume_from_seq?;
    // Seqs start at 1, so `0` is a client holding nothing at all — which
    // is a snapshot, not a resume.
    if from_seq == 0 {
        return None;
    }
    // A restarted daemon mints a fresh random epoch, so a stream from
    // before the restart cannot match here. That is the whole reason the
    // epoch is random rather than a counter: a client cannot resume onto
    // a different server's seq space even by accident.
    let server_epoch = h.supervisor.server_epoch()?;
    if handshake.server_epoch != Some(server_epoch) {
        return None;
    }
    // Channel and generation under one lock, so the client's claim is
    // checked against the pipeline this send is aimed at. It is not yet
    // a check on the pipeline that will *answer*: the command queues,
    // and a respawn can land behind it while the cloned sender keeps the
    // old task alive. That half is settled after the reply, by
    // [`generation_holds`].
    let (commands, live_generation) = h.supervisor.tab_task_handle(tab_id)?;
    if handshake.tab_generation != Some(live_generation) || live_generation != tab_generation {
        return None;
    }

    let (reply_tx, reply_rx) = oneshot::channel();
    commands
        .send(TabCmd::Resume {
            from_seq,
            reply: reply_tx,
        })
        .await
        .ok()?;
    let resumed = match reply_rx.await.ok()? {
        Ok(resumed) => resumed,
        Err(error) => {
            debug!(tab_id, from_seq, %error, "resume is unservable; falling back to a snapshot");
            return None;
        }
    };
    // Which pipeline served the hand-off, not which one the lookup
    // above found. A respawn between the two would otherwise stream the
    // old terminal's ring and live tail — usually straight into its
    // EXIT — to a client whose tab id now names the replacement.
    if !generation_holds(h, tab_id, resumed.tab_generation, tab_generation) {
        debug!(
            tab_id,
            from_seq,
            served = resumed.tab_generation,
            "the tab was respawned around the resume handoff; falling back to a snapshot"
        );
        return None;
    }

    Some(Attached {
        tee: resumed.receiver,
        commands,
        mode: AttachMode::Resume,
        seq: from_seq - 1,
        server_epoch,
        tab_generation: resumed.tab_generation,
        snapshot: Vec::new(),
        // The tab's size at the handoff, cut from the same turn of the
        // task as the ring slice. Read here rather than after the await —
        // a resize landing in between would describe records this reply
        // does not carry.
        reported_size: (resumed.cols, resumed.rows),
        owner: resumed.owner,
        grid_gen: resumed.grid_gen,
        ready_end: 0,
        terminator: false,
        snap_frame_bytes: MAX_DATA_FRAME_BYTES,
        replay: resumed.slice,
        stored_exit: resumed.stored_exit,
    })
}

async fn take_snapshot(
    commands: &mpsc::Sender<TabCmd>,
    kind: &AttachPayloadKind,
) -> Result<SnapshotAt, TabError> {
    let (reply_tx, reply_rx) = oneshot::channel();
    commands
        .send(TabCmd::Snapshot {
            kind: kind.clone(),
            reply: reply_tx,
        })
        .await
        .map_err(|_| TabError::Gone)?;
    // Bounded here and not only in the pump: a `vt` encode defers while
    // the parser sits mid-sequence with no retained continuation
    // (`vt_dump`'s step 9), so this wait covers a terminal that never
    // reaches a carryable state as well as the permit queue. Without it
    // the budget the pump measures from the fence would never be
    // reached, because the pump does not start until this returns.
    tokio::time::timeout(ATTACH_TIME_BUDGET, reply_rx)
        .await
        .map_err(|_| {
            TabError::SnapshotFailed(format!(
                "the snapshot did not arrive within {ATTACH_TIME_BUDGET:?}"
            ))
        })?
        .map_err(|_| TabError::Gone)?
}

/// Everything one live attach owns.
struct Pump {
    tab_id: i64,
    /// This attachment, as every size command it sends names it.
    id: AttachmentId,
    commands: mpsc::Sender<TabCmd>,
    tee: broadcast::Receiver<PtyOutputEvent>,
    writer: OwnedWriteHalf,
    close: ConnCloseWatch,
    /// `tab.drop_data_conns`: end as a disconnect, with no label.
    dropped: watch::Receiver<bool>,
    /// Empty on a resume, which is what makes the whole SNAP half of the
    /// pump inert: no frames, no budgets, no scheduling floors.
    snapshot: Vec<u8>,
    ready_end: usize,
    /// See [`Attached::terminator`].
    terminator: bool,
    /// See [`Attached::snap_frame_bytes`].
    snap_frame_bytes: usize,
    /// Ring records replayed as PTY frames before the live tee.
    replay: Vec<(u64, Vec<u8>)>,
    stored_exit: Option<(u64, i32)>,
    fence: u64,
    /// See [`Attached::grid_gen`].
    grid_gen: u64,
    /// When the attach's snapshot half began — at the fence, not at the
    /// first frame. [`ATTACH_TIME_BUDGET`] is measured from here.
    started: Instant,
}

/// How an attach finished. Only [`Ending::Fault`] and
/// [`Ending::Closed`] put a final `ERROR` frame on the wire; a client
/// that hung up gets nothing, because there is nobody to tell.
#[derive(Debug)]
enum Ending {
    /// EXIT was written, or the peer went away.
    Complete,
    ClientGone,
    /// The client sent `DETACH`. `trailing` names the frame that broke
    /// the rule that nothing follows it, which still gets its
    /// `protocol-error`.
    Detached {
        trailing: Option<String>,
    },
    /// `tab.drop_data_conns`: a disconnect nobody is told about.
    Dropped,
    /// A protocol or stream failure the peer is told about.
    Fault {
        code: &'static str,
        message: String,
    },
    /// The server closed this connection deliberately.
    Closed(CloseReason),
}

/// One frame's worth of news from the client's half of the socket.
enum ClientEvent {
    Frame(ClientFrame),
    /// The client broke the protocol: an oversized frame, a bogus
    /// length, a malformed payload, a frame after `DETACH`.
    Fatal(String),
    Eof,
}

/// A client frame, decoded and validated (`ipc.md`'s data plane).
#[derive(Debug)]
enum ClientFrame {
    Input { data: Vec<u8>, take: InputTake },
    Resize { geometry: Geometry, focused: bool },
    Take,
    Detach,
}

impl ClientFrame {
    /// `Ok(None)` is a frame the protocol says to ignore: a `RESIZE` to
    /// a zero-sized grid.
    fn decode(frame: DataFrame) -> Result<Option<ClientFrame>, String> {
        match frame.frame_type {
            FRAME_INPUT => {
                if frame.payload.len() < 2 {
                    return Err(format!(
                        "an INPUT frame carries a flag byte and at least one byte, got {}",
                        frame.payload.len()
                    ));
                }
                let take = InputTake::from_flags(frame.payload[0])?;
                let mut data = frame.payload;
                data.drain(..1);
                Ok(Some(ClientFrame::Input { data, take }))
            }
            FRAME_RESIZE => {
                let resize = ResizePayload::decode(&frame.payload)?;
                let geometry = Geometry {
                    cols: resize.cols,
                    rows: resize.rows,
                    cell_w: u32::from(resize.cell_w_px),
                    cell_h: u32::from(resize.cell_h_px),
                };
                // Dropped rather than applied or fatal: the handshake
                // refuses a zero-sized grid, and the two paths state one
                // client's geometry, so they have to agree about what a
                // grid is.
                if geometry.cols == 0 || geometry.rows == 0 {
                    debug!(
                        cols = geometry.cols,
                        rows = geometry.rows,
                        "ignored a RESIZE frame with a zero-sized grid"
                    );
                    return Ok(None);
                }
                Ok(Some(ClientFrame::Resize {
                    geometry,
                    focused: resize.focused,
                }))
            }
            FRAME_TAKE | FRAME_DETACH if !frame.payload.is_empty() => Err(format!(
                "a {} frame is empty, got {} bytes",
                if frame.frame_type == FRAME_TAKE {
                    "TAKE"
                } else {
                    "DETACH"
                },
                frame.payload.len()
            )),
            FRAME_TAKE => Ok(Some(ClientFrame::Take)),
            FRAME_DETACH => Ok(Some(ClientFrame::Detach)),
            other => Err(format!("unknown client frame type {other:#04x}")),
        }
    }
}

/// How many frames an attachment that is not yet committed may send
/// ahead of its commit — the same depth as the channel behind it. One
/// more is a protocol error: a reader that stopped at the cap would no
/// longer see the EOF or `DETACH` that has to cancel the attach.
const EARLY_FRAMES: usize = CLIENT_FRAME_CAPACITY;

/// Why an attach that was still being decided is not going ahead.
enum Cancel {
    /// EOF, or a socket that died.
    Gone,
    /// The client sent `DETACH`.
    Detached,
    /// The client broke the protocol; it is still reading JSON, so it is
    /// told with a rejection line.
    Fault(String),
}

impl Cancel {
    async fn answer(self, writer: &mut OwnedWriteHalf) {
        if let Cancel::Fault(message) = self {
            reject(writer, codes::PROTOCOL_ERROR, &message).await;
        }
    }
}

/// The client's half of the socket, read on its own task from the moment
/// the attach is admitted.
struct ClientFrames {
    rx: mpsc::Receiver<ClientEvent>,
    /// Frames that arrived before the commit, handled after it, in order.
    early: VecDeque<ClientFrame>,
    /// The reader's `DETACH` flag, set before the frame is handed over.
    detached: Arc<AtomicBool>,
}

impl ClientFrames {
    /// Start the reader, handing back its task: stopping it is the
    /// [`Detach`]'s business, which has to know when it has stopped.
    fn start(
        reader: DataFrameReader<OwnedReadHalf>,
        detached: Arc<AtomicBool>,
    ) -> (Self, tokio::task::JoinHandle<()>) {
        // The client's frames are read on their own task: a long
        // snapshot burst runs without awaiting the socket's read half,
        // and INPUT typed during it must not queue behind FINISH.
        let (tx, rx) = mpsc::channel(CLIENT_FRAME_CAPACITY);
        let reading = tokio::spawn(read_client(reader, tx, Arc::clone(&detached)));
        let frames = Self {
            rx,
            early: VecDeque::new(),
            detached,
        };
        (frames, reading)
    }

    /// Run one step of an attach that is not yet committed, watching the
    /// client while it runs: an EOF, a `DETACH` or a broken frame ends
    /// it before the step does — and still wins when it was ready beside
    /// the step's result. Other frames wait in [`Self::early`].
    async fn during<T>(&mut self, step: impl std::future::Future<Output = T>) -> Result<T, Cancel> {
        tokio::pin!(step);
        loop {
            tokio::select! {
                biased;
                out = &mut step => {
                    self.drain().await?;
                    return Ok(out);
                }
                event = self.rx.recv() => self.early_event(event).await?,
            }
        }
    }

    /// Everything the reader has already read, without waiting for the
    /// client to send more — including a `DETACH` read off the socket but
    /// not yet handed over.
    async fn drain(&mut self) -> Result<(), Cancel> {
        loop {
            let event = match self.rx.try_recv() {
                Ok(event) => Some(event),
                Err(mpsc::error::TryRecvError::Empty) if self.detached.load(Ordering::Acquire) => {
                    self.rx.recv().await
                }
                Err(mpsc::error::TryRecvError::Empty) => return Ok(()),
                Err(mpsc::error::TryRecvError::Disconnected) => None,
            };
            self.early_event(event).await?;
        }
    }

    /// One event before the commit: a frame waits, anything else cancels.
    async fn early_event(&mut self, event: Option<ClientEvent>) -> Result<(), Cancel> {
        match event {
            Some(ClientEvent::Frame(ClientFrame::Detach)) => Err(self.after_detach().await),
            Some(ClientEvent::Frame(_)) if self.early.len() == EARLY_FRAMES => Err(Cancel::Fault(
                format!("more than {EARLY_FRAMES} frames before the accepted reply"),
            )),
            Some(ClientEvent::Frame(frame)) => {
                self.early.push_back(frame);
                Ok(())
            }
            Some(ClientEvent::Fatal(message)) => Err(Cancel::Fault(message)),
            Some(ClientEvent::Eof) | None => Err(Cancel::Gone),
        }
    }

    /// A `DETACH` cancels; a frame the client had already written behind
    /// it still gets its `protocol-error`. The reader ends right after
    /// either, so this does not wait on the client.
    async fn after_detach(&mut self) -> Cancel {
        match self.rx.recv().await {
            Some(ClientEvent::Fatal(message)) => Cancel::Fault(message),
            _ => Cancel::Detached,
        }
    }

    /// The next frame without waiting: an early one first.
    fn try_next(&mut self) -> Option<ClientEvent> {
        if let Some(frame) = self.early.pop_front() {
            return Some(ClientEvent::Frame(frame));
        }
        self.rx.try_recv().ok()
    }

    /// The next frame. `None` once the reader has finished.
    async fn next(&mut self) -> Option<ClientEvent> {
        if let Some(frame) = self.early.pop_front() {
            return Some(ClientEvent::Frame(frame));
        }
        self.rx.recv().await
    }
}

impl Pump {
    #[allow(clippy::too_many_lines)] // One state machine; splitting it would hide the ordering.
    async fn run(mut self, client: &mut ClientFrames) -> Ending {
        let mut sent = 0usize;
        // The end-of-snapshot marker `vt` owes its client, still unsent.
        // Always false for GHOSTSNP, which is what makes every branch
        // below reduce to exactly today's behaviour there.
        let mut terminator_due = self.terminator;
        let mut pty_since_snap = 0usize;
        // PTY payload written while the snapshot was still going out.
        // It shares [`ATTACH_BYTE_BUDGET`] with the snapshot: what the
        // budget bounds is one attach's catch-up, and a client that
        // cannot keep up is no better off because half the bytes were
        // live ones.
        let mut pty_before_snap = 0usize;
        let mut last_snap = Instant::now();
        let mut tee = TeeState::new(self.fence, self.grid_gen);
        // Reused for every SNAP frame: a megabyte-chunked snapshot would
        // otherwise allocate once per frame for the whole catch-up.
        let mut snap = Vec::new();

        if self.snapshot.len() > ATTACH_BYTE_BUDGET {
            let message = format!(
                "the snapshot is {} bytes, past the {ATTACH_BYTE_BUDGET}-byte attach budget",
                self.snapshot.len()
            );
            return self
                .finish(Ending::Fault {
                    code: codes::DESYNC,
                    message,
                })
                .await;
        }

        // A resume starts with what the client missed, framed through
        // the very same fence walk the live tee goes through: the ring
        // slice must be contiguous from the fence too, so a hole in it
        // ends the connection instead of reaching a terminal that could
        // never tell. Nothing to do in snapshot mode — the replay is
        // empty and the loop below starts at the tee.
        if let Some(ending) = self.preload(&mut tee).await {
            return self.finish(ending).await;
        }

        let ending = 'pump: loop {
            // 0. Whether this connection is still open. Sticky and
            //    checked first, ahead of every drain: a client the
            //    server has closed — a stop, today the only reason —
            //    must stop receiving data, and stop writing input,
            //    within one pass rather than whenever the pump next
            //    happens to have nothing to do.
            if let Some(reason) = self.close.reason() {
                break Ending::Closed(reason);
            }
            if *self.dropped.borrow() {
                break Ending::Dropped;
            }

            // 1. Client frames already in hand, so input keeps flowing
            //    while the snapshot is being pumped. Bounded by the
            //    channel's own depth: a client that floods INPUT must
            //    not be able to hold the pass open indefinitely, which
            //    would starve the tee and defer the close check above.
            for _ in 0..CLIENT_FRAME_CAPACITY {
                let Some(event) = client.try_next() else {
                    break;
                };
                if let Some(ending) = self.handle_client(event, client).await {
                    break 'pump ending;
                }
            }

            // 2. What the tee has right now, up to one burst, framed
            //    into one write below rather than one syscall per
            //    record. Bounded for the same reason step 1 is, and
            //    because the snapshot's byte floor is measured per pass:
            //    an endless producer would otherwise never let the pass
            //    end. Whatever is left is taken next pass.
            // Until the READY prefix is fully out, tee records are
            // absorbed but HELD (§4.3 step 3: "queues the rest") — a
            // client has no terminal to apply a PTY frame to before
            // READY, and making it buffer them would move this queue to
            // every client. The hold window is tiny (the prefix is the
            // active screen, sent at full speed) and stays bounded by
            // the queue cap below; the burst floor is irrelevant while
            // holding because no interleaving decision exists yet.
            // The terminator keeps the hold open past the payload's
            // last byte: a PTY frame written between the two would put
            // live output ahead of the marker that says the snapshot is
            // over, and a client applying it would be writing into a
            // terminal it has not finished building.
            let holding = sent < self.ready_end || terminator_due;
            let mut drained_any = false;
            while tee.streaming() && (holding || tee.payload_bytes < PTY_BURST_BYTES) {
                match self.tee.try_recv() {
                    Ok(event) => {
                        drained_any = true;
                        if let Err(ending) = tee.absorb(event).await {
                            break 'pump ending;
                        }
                        // Inside the drain, not after it: the cap is
                        // about what is held for a peer that is not
                        // reading, so it may be overshot by the record
                        // that crossed it and by nothing more.
                        if tee.payload_bytes > FORWARDER_QUEUE_BYTES {
                            break 'pump Ending::Fault {
                                code: codes::OVERFLOW,
                                message: format!(
                                    "{} bytes are queued for a peer that is not reading",
                                    tee.payload_bytes
                                ),
                            };
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Closed) => {
                        tee.open = false;
                        break;
                    }
                    Err(TryRecvError::Lagged(missed)) => break 'pump lagged(missed),
                }
            }

            // 3. Flush what the tee gave us — once READY is out. PTY
            //    leads from then on: a keystroke's echo must not wait
            //    behind a scrollback page. This pass's payload is what
            //    step 5 weighs against the burst floor, so at most one
            //    burst of PTY precedes each SNAP.
            if !tee.batch.is_empty() && !holding {
                pty_since_snap += tee.payload_bytes;
                if sent < self.snapshot.len() {
                    pty_before_snap += tee.payload_bytes;
                }
                tee.payload_bytes = 0;
                if let Some(ending) = self.write_all(&tee.batch).await {
                    break ending;
                }
                tee.batch.clear();
            }

            // 4. EXIT is always the final frame, and it goes out only
            //    once everything before it has.
            if let Some((seq, code)) = tee.exit {
                if sent >= self.snapshot.len() && !terminator_due {
                    let mut payload = seq.to_le_bytes().to_vec();
                    payload.extend_from_slice(&code.to_le_bytes());
                    let mut frame = Vec::new();
                    let _ = write_data_frame(&mut frame, FRAME_EXIT, &payload).await;
                    if let Some(ending) = self.write_all(&frame).await {
                        break ending;
                    }
                    break Ending::Complete;
                }
            } else if !tee.open {
                break Ending::Fault {
                    code: codes::DESYNC,
                    message: "the tab's stream ended without an exit".into(),
                };
            }

            // 5. Snapshot progress.
            if sent < self.snapshot.len() || terminator_due {
                if self.started.elapsed() > ATTACH_TIME_BUDGET {
                    break Ending::Fault {
                        code: codes::DESYNC,
                        message: format!(
                            "the snapshot did not finish within {ATTACH_TIME_BUDGET:?}"
                        ),
                    };
                }
                let carried = self.snapshot.len().saturating_add(pty_before_snap);
                if carried > ATTACH_BYTE_BUDGET {
                    break Ending::Fault {
                        code: codes::DESYNC,
                        message: format!(
                            "this attach carried {carried} bytes before its snapshot \
                             finished, past the {ATTACH_BYTE_BUDGET}-byte attach budget"
                        ),
                    };
                }
                // Through READY at full speed; after it, PTY traffic
                // leads until one of the two floors is reached. Both are
                // weighed once per pass, and a pass absorbs at most
                // `PTY_BURST_BYTES` of payload (step 2) — so the burst
                // floor cannot be overrun by more than the record that
                // crossed it.
                // The terminator belongs to the head for the same
                // reason the payload does: it is the frame that tells a
                // `vt` client its terminal is complete, so pacing it
                // against PTY would delay the client's first paint.
                let head = sent < self.ready_end || terminator_due;
                let due = !drained_any
                    || pty_since_snap >= PTY_BURST_BYTES
                    || last_snap.elapsed() >= SNAP_PROGRESS_INTERVAL;
                if head || due {
                    // Frames stop at the READY boundary so it stays
                    // observable on the wire rather than being buried
                    // mid-frame.
                    // Deliberately NOT `head`: that one includes the
                    // terminator, and a kind whose ready prefix stopped
                    // short of its payload would then cap `limit` at a
                    // boundary already reached and make no progress.
                    let limit = if sent < self.ready_end {
                        self.ready_end
                    } else {
                        self.snapshot.len()
                    };
                    let end = limit.min(sent + self.snap_frame_bytes);
                    snap.clear();
                    // Guarded, not assumed: a `vt` payload always
                    // carries at least a cursor address, so `len == 0`
                    // does not arise today — but "there are bytes for
                    // this frame" and "the payload is fully out" are
                    // separate questions, and the marker below must not
                    // hang off the first one.
                    if end > sent {
                        let _ = write_data_frame(&mut snap, FRAME_SNAP, &self.snapshot[sent..end])
                            .await;
                    }
                    // Written into the SAME buffer as the final payload
                    // frame, never on a later pass: the next pass
                    // flushes held PTY and may write EXIT, and both "no
                    // PTY precedes the terminator" and "EXIT is last"
                    // have to hold.
                    if terminator_due && end == self.snapshot.len() {
                        let _ = write_data_frame(&mut snap, FRAME_SNAP, &[]).await;
                        terminator_due = false;
                    }
                    if let Some(ending) = self.write_all(&snap).await {
                        break ending;
                    }
                    sent = end;
                    pty_since_snap = 0;
                    last_snap = Instant::now();
                    continue;
                }
            }

            // 6. Nothing to do without waiting.
            let snap_due = last_snap + SNAP_PROGRESS_INTERVAL;
            let more_snapshot = sent < self.snapshot.len() || terminator_due;
            tokio::select! {
                biased;
                reason = self.close.closed() => break Ending::Closed(reason),
                Ok(()) = self.dropped.changed() => break Ending::Dropped,
                event = client.next() => match event {
                    Some(event) => if let Some(ending) = self.handle_client(event, client).await {
                        break ending;
                    },
                    // The reader task only ends after sending Eof or
                    // Fatal, so a closed channel means those were
                    // handled and the peer is gone.
                    None => break Ending::ClientGone,
                },
                event = self.tee.recv(), if tee.streaming() => match event {
                    Ok(event) => if let Err(ending) = tee.absorb(event).await {
                        break ending;
                    },
                    Err(RecvError::Closed) => tee.open = false,
                    Err(RecvError::Lagged(missed)) => break lagged(missed),
                },
                () = tokio::time::sleep_until(snap_due.into()), if more_snapshot => {}
            }
        };

        self.finish(ending).await
    }

    /// Frame the resume handoff's records into the tee state, ahead of
    /// anything the live subscription carries. `Some` ends the
    /// connection.
    async fn preload(&mut self, tee: &mut TeeState) -> Option<Ending> {
        for (seq, data) in std::mem::take(&mut self.replay) {
            if let Err(ending) = tee.absorb(PtyOutputEvent::Bytes { seq, data }).await {
                return Some(ending);
            }
        }
        // The tab died before this client came back, so its `Exit` is not
        // on the subscription — it was published before it existed — and
        // the handoff carried it instead. Absorbed like any other record
        // so it stays the final frame, after the slice.
        if let Some((seq, code)) = self.stored_exit {
            if let Err(ending) = tee.absorb(PtyOutputEvent::Exit { seq, code }).await {
                return Some(ending);
            }
        }
        None
    }

    /// Route one client frame. `Some` ends the connection.
    async fn handle_client(
        &mut self,
        event: ClientEvent,
        client: &mut ClientFrames,
    ) -> Option<Ending> {
        let frame = match event {
            ClientEvent::Frame(frame) => frame,
            ClientEvent::Fatal(message) => {
                return Some(Ending::Fault {
                    code: codes::PROTOCOL_ERROR,
                    message,
                })
            }
            ClientEvent::Eof => return Some(Ending::ClientGone),
        };
        // Every size question rides the tab task's one channel beside
        // the bytes, so a take is ordered against the input it goes
        // with without a fence. None is acknowledged on the wire.
        let id = self.id;
        match frame {
            ClientFrame::Input { data, take } => {
                self.send(TabCmd::Input {
                    data,
                    from: Some(id),
                    take,
                })
                .await;
            }
            ClientFrame::Resize { geometry, focused } => {
                self.send(TabCmd::Resize {
                    id,
                    geometry,
                    focused,
                })
                .await;
            }
            ClientFrame::Take => self.send(TabCmd::TakeFrom { id }).await,
            // Terminal. The reader stops at it, so the one event left
            // to read is whether anything was already sent behind it.
            ClientFrame::Detach => {
                let trailing = match client.next().await {
                    Some(ClientEvent::Fatal(message)) => Some(message),
                    _ => None,
                };
                return Some(Ending::Detached { trailing });
            }
        }
        None
    }

    /// Hand a command to the tab task. A tab that is gone will announce
    /// itself through the tee's `Exit`, so a failed send is logged, not
    /// escalated.
    async fn send(&self, cmd: TabCmd) {
        if self.commands.send(cmd).await.is_err() {
            debug!(
                tab_id = self.tab_id,
                "dropped a client frame: the tab task is gone"
            );
        }
    }

    /// `Some` when the write failed or stalled, which ends the
    /// connection.
    async fn write_all(&mut self, bytes: &[u8]) -> Option<Ending> {
        let write = async {
            self.writer.write_all(bytes).await?;
            self.writer.flush().await
        };
        match tokio::time::timeout(WRITE_DEADLINE, write).await {
            Ok(Ok(())) => None,
            Ok(Err(error)) => {
                debug!(tab_id = self.tab_id, %error, "attach write failed");
                Some(Ending::ClientGone)
            }
            Err(_) => Some(Ending::Fault {
                code: codes::OVERFLOW,
                message: format!("the peer did not read a frame within {WRITE_DEADLINE:?}"),
            }),
        }
    }

    /// Label the close where there is still somebody to tell. Purely
    /// best-effort: a peer that stopped reading made this write
    /// impossible the moment its socket buffer filled, and EOF is then
    /// the only signal it gets.
    async fn finish(mut self, ending: Ending) -> Ending {
        let error = match &ending {
            Ending::Complete
            | Ending::ClientGone
            | Ending::Dropped
            | Ending::Detached { trailing: None } => return ending,
            Ending::Detached {
                trailing: Some(message),
            } => ResponseError {
                code: codes::PROTOCOL_ERROR.to_string(),
                message: message.clone(),
            },
            Ending::Fault { code, message } => ResponseError {
                code: (*code).to_string(),
                message: message.clone(),
            },
            Ending::Closed(reason) => ResponseError {
                code: reason.error_code().to_string(),
                message: format!("this data connection was closed: {}", reason.error_code()),
            },
        };
        if !matches!(ending, Ending::Closed(_)) {
            warn!(tab_id = self.tab_id, code = %error.code, message = %error.message, "attach failed");
        }
        let payload = match serde_json::to_vec(&error) {
            Ok(payload) => payload,
            Err(e) => {
                warn!(
                    tab_id = self.tab_id,
                    path = "attach-label-encode-failed",
                    code = %error.code,
                    error = %e,
                    "the final ERROR frame failed to serialize; closing unlabeled"
                );
                return ending;
            }
        };
        let mut frame = Vec::new();
        if let Err(e) = write_data_frame(&mut frame, FRAME_ERROR, &payload).await {
            warn!(
                tab_id = self.tab_id,
                path = "attach-label-frame-failed",
                code = %error.code,
                error = %e,
                "the final ERROR frame could not be framed; closing unlabeled"
            );
            return ending;
        }
        let write = async {
            self.writer.write_all(&frame).await?;
            self.writer.flush().await
        };
        match tokio::time::timeout(CLOSE_LABEL_DEADLINE, write).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!(
                tab_id = self.tab_id,
                path = "attach-label-write-failed",
                code = %error.code,
                error = %e,
                "the final ERROR frame could not be written; closing unlabeled"
            ),
            Err(_) => warn!(
                tab_id = self.tab_id,
                path = "attach-label-deadline",
                code = %error.code,
                deadline_ms = CLOSE_LABEL_DEADLINE.as_millis(),
                "the final ERROR frame stalled; closing unlabeled"
            ),
        }
        ending
    }
}

/// The tee side of one attach: how far the fence walk has got, and the
/// PTY and GRID frames it has framed but not yet written.
struct TeeState {
    fence: u64,
    /// `GRID`s at or below this generation describe a state the accepted
    /// reply already carried.
    grid_gen: u64,
    next_seq: u64,
    open: bool,
    exit: Option<(u64, i32)>,
    /// Framed PTY frames, written and cleared as one batch — one write
    /// per drain rather than one per record. Kept across flushes so a
    /// busy tab reframes into the same allocation.
    batch: Vec<u8>,
    /// Payload bytes sitting in `batch`, GRID included, which is what
    /// both the queue cap and the snapshot's byte floor are counted in.
    payload_bytes: usize,
    /// `u64-LE seq | raw PTY bytes` for the record being framed, reused
    /// for the same reason `batch` is.
    scratch: Vec<u8>,
}

impl TeeState {
    fn new(fence: u64, grid_gen: u64) -> Self {
        Self {
            fence,
            grid_gen,
            next_seq: fence + 1,
            open: true,
            exit: None,
            batch: Vec::new(),
            payload_bytes: 0,
            scratch: Vec::new(),
        }
    }

    /// Whether reading the tee again can still produce anything: `Exit`
    /// is the last record a tab ever publishes.
    fn streaming(&self) -> bool {
        self.open && self.exit.is_none()
    }

    /// Apply the fence and the contiguity rule to one tee event, framing
    /// what survives into [`TeeState::batch`]. A `GRID` is outside both:
    /// it repeats the seq of the record before it, so it is filtered by
    /// its generation instead and framed in its place in the stream.
    async fn absorb(&mut self, event: PtyOutputEvent) -> Result<(), Ending> {
        match event {
            PtyOutputEvent::Grid {
                seq,
                gen,
                cols,
                rows,
                owner,
            } => {
                if gen <= self.grid_gen {
                    return Ok(());
                }
                self.grid_gen = gen;
                let payload = serde_json::to_vec(&GridFrame {
                    seq,
                    cols,
                    rows,
                    owner,
                })
                .map_err(|error| Ending::Fault {
                    code: codes::INTERNAL,
                    message: format!("a GRID frame could not be encoded: {error}"),
                })?;
                if write_data_frame(&mut self.batch, FRAME_GRID, &payload)
                    .await
                    .is_err()
                {
                    return Err(Ending::Fault {
                        code: codes::INTERNAL,
                        message: "a GRID frame exceeded the frame cap".into(),
                    });
                }
                self.payload_bytes += payload.len();
            }
            PtyOutputEvent::Bytes { seq, data } => {
                if !self.admit(seq)? {
                    return Ok(());
                }
                self.scratch.clear();
                self.scratch.extend_from_slice(&seq.to_le_bytes());
                self.scratch.extend_from_slice(&data);
                if write_data_frame(&mut self.batch, FRAME_PTY, &self.scratch)
                    .await
                    .is_err()
                {
                    return Err(Ending::Fault {
                        code: codes::DESYNC,
                        message: "a PTY record exceeded the frame cap".into(),
                    });
                }
                self.payload_bytes += self.scratch.len();
            }
            PtyOutputEvent::Exit { seq, code } => {
                if !self.admit(seq)? {
                    return Ok(());
                }
                self.exit = Some((seq, code));
                self.open = false;
            }
        }
        Ok(())
    }

    /// The fence and the contiguity rule for one record: `false` for one
    /// already inside the snapshot, a fault for a gap or a duplicate.
    fn admit(&mut self, seq: u64) -> Result<bool, Ending> {
        if seq <= self.fence {
            return Ok(false);
        }
        if seq != self.next_seq {
            return Err(Ending::Fault {
                code: codes::DESYNC,
                message: format!("expected seq {} on this tab, got {seq}", self.next_seq),
            });
        }
        self.next_seq += 1;
        Ok(true)
    }
}

/// A skipped record is fatal by contract, from either side of the tee: a
/// hole in the client's terminal is one it can never fill in.
fn lagged(missed: u64) -> Ending {
    Ending::Fault {
        code: codes::DESYNC,
        message: format!("this attach fell {missed} records behind its tab"),
    }
}

/// Read the client's half until it ends, forwarding decoded frames.
///
/// It stops at `DETACH`, which nothing may follow: whatever the client
/// had already written behind it is reported as one `Fatal`, and nothing
/// further is read.
async fn read_client(
    mut reader: DataFrameReader<OwnedReadHalf>,
    tx: mpsc::Sender<ClientEvent>,
    detached: Arc<AtomicBool>,
) {
    loop {
        let event = match reader.next_frame().await {
            Ok(Some(frame)) => match ClientFrame::decode(frame) {
                Ok(Some(frame)) => ClientEvent::Frame(frame),
                Ok(None) => continue,
                Err(message) => ClientEvent::Fatal(message),
            },
            Ok(None) => ClientEvent::Eof,
            // An oversized or bogus length is the client breaking the
            // protocol and is named as such; anything else is a socket
            // that died, which is just an EOF with extra steps.
            Err(roost_ipc::Error::DataFrameTooLarge) => ClientEvent::Fatal(format!(
                "a client frame claimed more than the {MAX_DATA_FRAME_BYTES}-byte cap"
            )),
            Err(_) => ClientEvent::Eof,
        };
        let detach = matches!(event, ClientEvent::Frame(ClientFrame::Detach));
        if detach {
            detached.store(true, Ordering::Release);
        }
        let done = matches!(event, ClientEvent::Eof | ClientEvent::Fatal(_));
        if tx.send(event).await.is_err() || done {
            return;
        }
        if detach {
            if reader.has_buffered() {
                let _ = tx
                    .send(ClientEvent::Fatal("a frame followed DETACH".into()))
                    .await;
            }
            return;
        }
    }
}

/// The one line a refused data connection gets, in the same shape an
/// accepted one's reply has.
async fn reject(writer: &mut OwnedWriteHalf, code: &str, message: &str) {
    let Ok(body) = serde_json::to_vec(&AttachHandshakeReply::rejected(code, message)) else {
        return;
    };
    if let Err(error) = write_frame(writer, &body).await {
        debug!(%error, "attach rejection could not be written");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roost_ipc::dataframe::FRAME_HEADER_LEN;

    /// The forwarder's half of a connection, and the client's write half.
    fn client_frames() -> (ClientFrames, Arc<AtomicBool>, OwnedWriteHalf) {
        let (client, server) = tokio::net::UnixStream::pair().expect("socket pair");
        let (read, _) = server.into_split();
        let (_, write) = client.into_split();
        let detached = Arc::new(AtomicBool::new(false));
        let (frames, _reading) = ClientFrames::start(
            DataFrameReader::new(read, Vec::new()),
            Arc::clone(&detached),
        );
        (frames, detached, write)
    }

    async fn send(write: &mut OwnedWriteHalf, frames: &[(u8, &[u8])]) {
        let mut bytes = Vec::new();
        for (frame_type, payload) in frames {
            write_data_frame(&mut bytes, *frame_type, payload)
                .await
                .expect("frame");
        }
        write.write_all(&bytes).await.expect("write");
    }

    async fn read_by_the_reader(detached: &AtomicBool) {
        while !detached.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    }

    /// A reader still between decoding a `DETACH` and recording it — where
    /// no abort reaches — is waited out before the `Detach` says whether
    /// it was deliberate.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_detach_waits_for_its_reader_to_finish_recording() {
        let (commands, mut tab) = mpsc::channel(4);
        let detached = Arc::new(AtomicBool::new(false));
        let decoding = Arc::new(std::sync::Barrier::new(2));
        let reader = tokio::spawn({
            let (detached, decoding) = (Arc::clone(&detached), Arc::clone(&decoding));
            async move {
                decoding.wait();
                std::thread::sleep(Duration::from_millis(200));
                detached.store(true, Ordering::Release);
            }
        });
        decoding.wait();
        Detach {
            tab_id: 1,
            id: 7,
            commands,
            detached,
            reader: Some(reader),
        }
        .send();
        let sent = tokio::time::timeout(Duration::from_secs(5), tab.recv())
            .await
            .expect("a detach in time");
        assert!(
            matches!(
                sent,
                Some(TabCmd::Detach {
                    id: 7,
                    deliberate: true
                })
            ),
            "the DETACH the reader was recording counts"
        );
    }

    /// A forwarder cancelled while it waits for its reader to stop still
    /// leaves the guard a reader to wait out.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reader_wait_cut_short_is_waited_out_by_the_detach() {
        let (commands, mut tab) = mpsc::channel(4);
        let detached = Arc::new(AtomicBool::new(false));
        let decoding = Arc::new(std::sync::Barrier::new(2));
        let reader = tokio::spawn({
            let (detached, decoding) = (Arc::clone(&detached), Arc::clone(&decoding));
            async move {
                decoding.wait();
                std::thread::sleep(Duration::from_millis(200));
                detached.store(true, Ordering::Release);
            }
        });
        decoding.wait();
        let mut detach = Detach {
            tab_id: 1,
            id: 7,
            commands,
            detached,
            reader: Some(reader),
        };
        assert!(
            tokio::time::timeout(Duration::from_millis(20), detach.stop_reader())
                .await
                .is_err(),
            "the reader is still recording"
        );
        detach.send();
        let sent = tokio::time::timeout(Duration::from_secs(5), tab.recv())
            .await
            .expect("a detach in time");
        assert!(
            matches!(
                sent,
                Some(TabCmd::Detach {
                    id: 7,
                    deliberate: true
                })
            ),
            "the DETACH the reader was recording counts"
        );
    }

    /// A `DETACH` already handed over when a step finishes cancels the
    /// attach, though the step's result was ready first.
    #[tokio::test]
    async fn a_waiting_detach_beats_a_finished_step() {
        let (mut frames, detached, mut write) = client_frames();
        send(&mut write, &[(FRAME_DETACH, &[])]).await;
        read_by_the_reader(&detached).await;
        assert!(matches!(
            frames.during(std::future::ready(())).await,
            Err(Cancel::Detached)
        ));
    }

    /// One the reader has read but could not yet hand over — its channel
    /// full of early frames — cancels too.
    #[tokio::test]
    async fn a_detach_read_but_not_yet_handed_over_cancels() {
        let (mut frames, detached, mut write) = client_frames();
        let mut sent: Vec<(u8, &[u8])> = vec![(FRAME_INPUT, b"\0x"); CLIENT_FRAME_CAPACITY];
        sent.push((FRAME_DETACH, &[]));
        send(&mut write, &sent).await;
        read_by_the_reader(&detached).await;
        assert!(matches!(frames.drain().await, Err(Cancel::Detached)));
    }

    fn grid(seq: u64, gen: u64) -> PtyOutputEvent {
        PtyOutputEvent::Grid {
            seq,
            gen,
            cols: 100,
            rows: 30,
            owner: None,
        }
    }

    /// A `GRID` at or below the reply's generation describes a state the
    /// reply already carried, so it is dropped; a newer one is framed in
    /// its place, outside the seq rule the records beside it are under.
    #[tokio::test]
    async fn a_grid_the_reply_already_carried_is_dropped() {
        let mut tee = TeeState::new(5, 3);
        assert!(tee.absorb(grid(4, 2)).await.is_ok());
        assert!(tee.absorb(grid(5, 3)).await.is_ok());
        assert!(tee.batch.is_empty(), "both are inside the reply");

        assert!(tee.absorb(grid(5, 4)).await.is_ok());
        assert_eq!(tee.batch[4], FRAME_GRID, "a newer GRID is framed");
        let framed = tee.batch.len();
        assert!(
            tee.absorb(PtyOutputEvent::Bytes {
                seq: 6,
                data: b"x".to_vec(),
            })
            .await
            .is_ok(),
            "the GRID consumed no seq"
        );
        assert_eq!(tee.batch[framed + 4], FRAME_PTY);
        assert!(tee.absorb(grid(6, 4)).await.is_ok());
        assert_eq!(
            tee.batch.len(),
            framed + FRAME_HEADER_LEN + 9,
            "a generation is framed once"
        );
    }
}
