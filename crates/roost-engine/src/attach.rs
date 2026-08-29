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
//! tab, a seq the ring no longer covers, or a tab task that went away all
//! fall back to a full snapshot **in the same reply**. A client is told
//! which it got by `mode`, and never has to handle a resume failure.

use std::time::{Duration, Instant};

use roost_ipc::dataframe::{
    write_data_frame, write_preamble, DataFrame, DataFrameReader, FRAME_ERROR, FRAME_EXIT,
    FRAME_INPUT, FRAME_PTY, FRAME_RESIZE, FRAME_SNAP, MAX_DATA_FRAME_BYTES,
};
use roost_ipc::framing::write_frame;
use roost_ipc::messages::{
    AttachAccepted, AttachHandshake, AttachHandshakeReply, AttachMode, AttachPayloadKind,
    ResponseError, SESSION_PROTOCOL_VERSION,
};
use roost_ipc::{CloseReason, ConnCloseWatch, ConnCtx, DataConn, CLOSE_LABEL_DEADLINE};
use tokio::io::AsyncWriteExt;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, warn};

use crate::ipc::IpcHandler;
use crate::pty::PtyOutputEvent;
use crate::tab_task::{SnapshotAt, TabCmd, TabError};

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

/// Serve one data connection: admit it, fence it, and stream its tab
/// until something ends it.
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

    // Before the token is even looked at: a version mismatch means the
    // two ends disagree about what a token *is*, and answering
    // `invalid-token` would send the client hunting for the wrong bug.
    if handshake.protocol_version != SESSION_PROTOCOL_VERSION {
        reject(
            &mut writer,
            "protocol-mismatch",
            &format!(
                "this session speaks session protocol {SESSION_PROTOCOL_VERSION}; \
                 the client offered {}",
                handshake.protocol_version
            ),
        )
        .await;
        return;
    }

    let admitted = match h.admit_attach(&handshake.attach, ctx) {
        Ok(admitted) => admitted,
        Err(error) => {
            // The token is never echoed: it is a bearer credential, and
            // an error message is a log line waiting to happen.
            reject(&mut writer, &error.code, &error.message).await;
            return;
        }
    };

    let tab_id = admitted.tab_id;
    let outcome = attach_tab(
        h,
        admitted.tab_generation,
        tab_id,
        &handshake,
        reader,
        writer,
        close,
    );
    // Registered under the lease by the admission; deregistered here
    // however the forwarder ended, so a tab that is attached and
    // detached repeatedly does not accumulate entries.
    outcome.await;
    h.release_data_conn(tab_id, ctx.conn_id);
}

/// Fence the tab, answer the handshake, and pump until the end.
async fn attach_tab(
    h: &IpcHandler,
    tab_generation: u64,
    tab_id: i64,
    handshake: &AttachHandshake,
    reader: DataFrameReader<OwnedReadHalf>,
    mut writer: OwnedWriteHalf,
    close: ConnCloseWatch,
) {
    // Started before the snapshot is asked for, not after it arrives:
    // the encode queues behind `MAX_CONCURRENT_SNAPSHOTS` and is exactly
    // the part of an attach the time budget exists to bound.
    let started = Instant::now();
    // Resume first, and only ever as an optimization: everything it
    // cannot honor comes back `None` and is served as a full attach
    // under the same reply (D6).
    let attached = match resume_tab(h, tab_generation, tab_id, handshake).await {
        Some(attached) => attached,
        None => match fence_tab(h, tab_generation, tab_id).await {
            Ok(fenced) => fenced,
            Err((code, message)) => {
                reject(&mut writer, code, &message).await;
                return;
            }
        },
    };
    let Attached {
        tee,
        commands,
        mode,
        seq: fence,
        server_epoch,
        tab_generation: live_generation,
        snapshot,
        ready_end,
        replay,
        stored_exit,
    } = attached;

    // The fence is a round trip through the tab task, and a supersede or
    // a takeover during it means this client lost authority before it
    // ever saw a byte. Answering `accepted` now would hand it a stream
    // it must not have.
    if close.reason().is_some() {
        debug!(
            tab_id,
            "the data connection closed while its tab was fenced"
        );
        return;
    }

    let accepted = AttachHandshakeReply::Accepted(AttachAccepted {
        kind: AttachPayloadKind::from(AttachPayloadKind::GHOSTTY_SNAPSHOT),
        mode,
        seq: fence,
        server_epoch,
        tab_generation: live_generation,
    });
    let Ok(body) = serde_json::to_vec(&accepted) else {
        return;
    };
    if write_frame(&mut writer, &body).await.is_err() {
        return;
    }
    if write_preamble(&mut writer).await.is_err() {
        return;
    }

    let ending = Pump {
        tab_id,
        commands,
        tee,
        writer,
        close,
        snapshot,
        ready_end,
        replay,
        stored_exit,
        fence,
        started,
    }
    .run(reader)
    .await;
    debug!(tab_id, ?mode, ?ending, "attach data connection ended");
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
    ready_end: usize,
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
) -> Result<Attached, (&'static str, String)> {
    let no_terminal = || ("not-found", "that tab has no live terminal".to_string());
    // Subscribed FIRST, before the snapshot is even asked for: the
    // encode runs on the tab task between two chunks, so a subscription
    // taken afterwards could miss whatever landed in between. Discarding
    // pre-fence events is cheap; recovering a lost one is impossible.
    let tee = h
        .supervisor
        .subscribe_output(tab_id)
        .ok_or_else(no_terminal)?;
    // Channel and generation in one lock acquisition, so the task this
    // snapshots is provably the task whose generation is checked. A
    // respawn between the subscribe above and this read gives a new
    // generation, which the check below turns into a clean `not-found`
    // rather than a tee and a snapshot from two different terminals.
    let (commands, live_generation) = h
        .supervisor
        .tab_task_handle(tab_id)
        .ok_or_else(no_terminal)?;
    if live_generation != tab_generation {
        return Err((
            "not-found",
            "that tab was respawned after the attach token was issued".to_string(),
        ));
    }

    let snapshot = take_snapshot(&commands)
        .await
        .map_err(|error| match error {
            // Re-attach is the recovery: the failure is about the terminal's
            // state at this instant, not about the client.
            TabError::SnapshotFailed(why) => ("snapshot-failed", why),
            other => ("not-found", other.to_string()),
        })?;

    // The token named a tab pipeline, not just a tab id. A respawn
    // between `tab.attach` and here is a different terminal with a
    // different seq space, and streaming it under the old identity is
    // exactly what the generation exists to prevent.
    if snapshot.tab_generation != tab_generation {
        return Err((
            "not-found",
            "that tab was respawned after the attach token was issued".to_string(),
        ));
    }

    let ready_end = roost_vt::ready_boundary(&snapshot.bytes).map_err(|error| {
        (
            "snapshot-failed",
            format!("the encoded snapshot has no READY record: {error}"),
        )
    })?;

    Ok(Attached {
        tee,
        commands,
        mode: AttachMode::Snapshot,
        seq: snapshot.seq,
        server_epoch: snapshot.server_epoch,
        tab_generation: snapshot.tab_generation,
        snapshot: snapshot.bytes,
        ready_end,
        replay: Vec::new(),
        stored_exit: None,
    })
}

/// The resume handoff (D6), or `None` for every reason it cannot be
/// honored — a stale epoch, a respawned tab, a seq outside the ring, a
/// tab task that is gone. Never an error: the caller serves a full
/// snapshot instead, in the same reply, and a client that has been away
/// too long simply pays for a snapshot rather than being told off.
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
    // Channel and generation under one lock, so the pipeline that
    // answers `Resume` is provably the one whose identity was just
    // checked — against what the client claims AND against what the
    // token was minted for. The second half of that has no test: it
    // needs the same `tab_id` respawned between `tab.attach` and the
    // handshake, and ids are never reused. It is the resume twin of the
    // snapshot path's own generation re-check, and it stays for the same
    // reason: streaming a second terminal's seq space under the first's
    // identity is the exact thing the generation exists to prevent.
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

    Some(Attached {
        tee: resumed.receiver,
        commands,
        mode: AttachMode::Resume,
        seq: from_seq - 1,
        server_epoch,
        tab_generation: live_generation,
        snapshot: Vec::new(),
        ready_end: 0,
        replay: resumed.slice,
        stored_exit: resumed.stored_exit,
    })
}

async fn take_snapshot(commands: &mpsc::Sender<TabCmd>) -> Result<SnapshotAt, TabError> {
    let (reply_tx, reply_rx) = oneshot::channel();
    commands
        .send(TabCmd::Snapshot(reply_tx))
        .await
        .map_err(|_| TabError::Gone)?;
    reply_rx.await.map_err(|_| TabError::Gone)?
}

/// Everything one live attach owns.
struct Pump {
    tab_id: i64,
    commands: mpsc::Sender<TabCmd>,
    tee: broadcast::Receiver<PtyOutputEvent>,
    writer: OwnedWriteHalf,
    close: ConnCloseWatch,
    /// Empty on a resume, which is what makes the whole SNAP half of the
    /// pump inert: no frames, no budgets, no scheduling floors.
    snapshot: Vec<u8>,
    ready_end: usize,
    /// Ring records replayed as PTY frames before the live tee.
    replay: Vec<(u64, Vec<u8>)>,
    stored_exit: Option<(u64, i32)>,
    fence: u64,
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
    Frame(DataFrame),
    /// The framing itself failed — an oversized frame, a bogus length.
    Fatal(String),
    Eof,
}

impl Pump {
    #[allow(clippy::too_many_lines)] // One state machine; splitting it would hide the ordering.
    async fn run(mut self, reader: DataFrameReader<OwnedReadHalf>) -> Ending {
        // The client's frames are read on their own task: a long
        // snapshot burst runs without awaiting the socket's read half,
        // and INPUT typed during it must not queue behind FINISH.
        let (client_tx, mut client_rx) = mpsc::channel(CLIENT_FRAME_CAPACITY);
        let client_task = tokio::spawn(read_client(reader, client_tx));

        let mut sent = 0usize;
        let mut pty_since_snap = 0usize;
        // PTY payload written while the snapshot was still going out.
        // It shares [`ATTACH_BYTE_BUDGET`] with the snapshot: what the
        // budget bounds is one attach's catch-up, and a client that
        // cannot keep up is no better off because half the bytes were
        // live ones.
        let mut pty_before_snap = 0usize;
        let mut last_snap = Instant::now();
        let mut tee = TeeState::new(self.fence);
        // Reused for every SNAP frame: a megabyte-chunked snapshot would
        // otherwise allocate once per frame for the whole catch-up.
        let mut snap = Vec::new();

        if self.snapshot.len() > ATTACH_BYTE_BUDGET {
            client_task.abort();
            let message = format!(
                "the snapshot is {} bytes, past the {ATTACH_BYTE_BUDGET}-byte attach budget",
                self.snapshot.len()
            );
            return self
                .finish(Ending::Fault {
                    code: "desync",
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
            client_task.abort();
            return self.finish(ending).await;
        }

        let ending = 'pump: loop {
            // 0. Whether this connection still has authority. Sticky and
            //    checked first, ahead of every drain: a superseded or
            //    taken-over client must stop receiving data — and lose
            //    its input authority — within one pass, not whenever the
            //    pump next happens to have nothing to do.
            if let Some(reason) = self.close.reason() {
                break Ending::Closed(reason);
            }

            // 1. Client frames already in hand, so input keeps flowing
            //    while the snapshot is being pumped. Bounded by the
            //    channel's own depth: a client that floods INPUT must
            //    not be able to hold the pass open indefinitely, which
            //    would starve the tee and defer the close check above.
            for _ in 0..CLIENT_FRAME_CAPACITY {
                let Ok(event) = client_rx.try_recv() else {
                    break;
                };
                if let Some(ending) = self.handle_client(event).await {
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
            let holding = sent < self.ready_end;
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
                                code: "overflow",
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
                if sent >= self.snapshot.len() {
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
                    code: "desync",
                    message: "the tab's stream ended without an exit".into(),
                };
            }

            // 5. Snapshot progress.
            if sent < self.snapshot.len() {
                if self.started.elapsed() > ATTACH_TIME_BUDGET {
                    break Ending::Fault {
                        code: "desync",
                        message: format!(
                            "the snapshot did not finish within {ATTACH_TIME_BUDGET:?}"
                        ),
                    };
                }
                let carried = self.snapshot.len().saturating_add(pty_before_snap);
                if carried > ATTACH_BYTE_BUDGET {
                    break Ending::Fault {
                        code: "desync",
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
                let head = sent < self.ready_end;
                let due = !drained_any
                    || pty_since_snap >= PTY_BURST_BYTES
                    || last_snap.elapsed() >= SNAP_PROGRESS_INTERVAL;
                if head || due {
                    // Frames stop at the READY boundary so it stays
                    // observable on the wire rather than being buried
                    // mid-frame.
                    let limit = if head {
                        self.ready_end
                    } else {
                        self.snapshot.len()
                    };
                    let end = limit.min(sent + MAX_DATA_FRAME_BYTES);
                    snap.clear();
                    let _ =
                        write_data_frame(&mut snap, FRAME_SNAP, &self.snapshot[sent..end]).await;
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
            let more_snapshot = sent < self.snapshot.len();
            tokio::select! {
                biased;
                reason = self.close.closed() => break Ending::Closed(reason),
                event = client_rx.recv() => match event {
                    Some(event) => if let Some(ending) = self.handle_client(event).await {
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

        client_task.abort();
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
    async fn handle_client(&mut self, event: ClientEvent) -> Option<Ending> {
        let frame = match event {
            ClientEvent::Frame(frame) => frame,
            ClientEvent::Fatal(message) => {
                return Some(Ending::Fault {
                    code: "protocol-error",
                    message,
                })
            }
            ClientEvent::Eof => return Some(Ending::ClientGone),
        };
        match frame.frame_type {
            FRAME_INPUT => {
                if frame.payload.is_empty() {
                    return Some(Ending::Fault {
                        code: "protocol-error",
                        message: "an INPUT frame carried no bytes".into(),
                    });
                }
                self.send(TabCmd::Input(frame.payload)).await;
            }
            FRAME_RESIZE => {
                // Fixed width, so a short or long payload is a client
                // bug rather than something to interpret generously.
                let Ok(fields) = <[u8; 8]>::try_from(frame.payload.as_slice()) else {
                    return Some(Ending::Fault {
                        code: "protocol-error",
                        message: format!(
                            "a RESIZE frame must carry exactly 8 bytes, got {}",
                            frame.payload.len()
                        ),
                    });
                };
                let read = |at: usize| u16::from_le_bytes([fields[at], fields[at + 1]]);
                // No ack: a RESIZE frame is unacknowledged on the wire,
                // so there is nobody to tell when it lands.
                self.send(TabCmd::Resize {
                    cols: read(0),
                    rows: read(2),
                    cell_w: u32::from(read(4)),
                    cell_h: u32::from(read(6)),
                    ack: None,
                })
                .await;
            }
            other => {
                return Some(Ending::Fault {
                    code: "protocol-error",
                    message: format!("unknown client frame type {other:#04x}"),
                })
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
                code: "overflow",
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
            Ending::Complete | Ending::ClientGone => return ending,
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
        let Ok(payload) = serde_json::to_vec(&error) else {
            return ending;
        };
        let mut frame = Vec::new();
        if write_data_frame(&mut frame, FRAME_ERROR, &payload)
            .await
            .is_err()
        {
            return ending;
        }
        let write = async {
            let _ = self.writer.write_all(&frame).await;
            let _ = self.writer.flush().await;
        };
        if tokio::time::timeout(CLOSE_LABEL_DEADLINE, write)
            .await
            .is_err()
        {
            debug!(
                tab_id = self.tab_id,
                "the final ERROR frame stalled; closing unlabeled"
            );
        }
        ending
    }
}

/// The tee side of one attach: how far the fence walk has got, and the
/// PTY frames it has framed but not yet written.
struct TeeState {
    fence: u64,
    next_seq: u64,
    open: bool,
    exit: Option<(u64, i32)>,
    /// Framed PTY frames, written and cleared as one batch — one write
    /// per drain rather than one per record. Kept across flushes so a
    /// busy tab reframes into the same allocation.
    batch: Vec<u8>,
    /// Payload bytes sitting in `batch`, which is what both the queue
    /// cap and the snapshot's byte floor are counted in.
    payload_bytes: usize,
    /// `u64-LE seq | raw PTY bytes` for the record being framed, reused
    /// for the same reason `batch` is.
    scratch: Vec<u8>,
}

impl TeeState {
    fn new(fence: u64) -> Self {
        Self {
            fence,
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
    /// what survives into [`TeeState::batch`].
    async fn absorb(&mut self, event: PtyOutputEvent) -> Result<(), Ending> {
        let seq = event.seq();
        // Already inside the snapshot.
        if seq <= self.fence {
            return Ok(());
        }
        if seq != self.next_seq {
            return Err(Ending::Fault {
                code: "desync",
                message: format!("expected seq {} on this tab, got {seq}", self.next_seq),
            });
        }
        self.next_seq += 1;
        match event {
            PtyOutputEvent::Bytes { seq, data } => {
                self.scratch.clear();
                self.scratch.extend_from_slice(&seq.to_le_bytes());
                self.scratch.extend_from_slice(&data);
                if write_data_frame(&mut self.batch, FRAME_PTY, &self.scratch)
                    .await
                    .is_err()
                {
                    return Err(Ending::Fault {
                        code: "desync",
                        message: "a PTY record exceeded the frame cap".into(),
                    });
                }
                self.payload_bytes += self.scratch.len();
            }
            PtyOutputEvent::Exit { seq, code } => {
                self.exit = Some((seq, code));
                self.open = false;
            }
        }
        Ok(())
    }
}

/// A skipped record is fatal by contract, from either side of the tee: a
/// hole in the client's terminal is one it can never fill in.
fn lagged(missed: u64) -> Ending {
    Ending::Fault {
        code: "desync",
        message: format!("this attach fell {missed} records behind its tab"),
    }
}

/// Read the client's half until it ends, forwarding frames to the pump.
async fn read_client(mut reader: DataFrameReader<OwnedReadHalf>, tx: mpsc::Sender<ClientEvent>) {
    loop {
        let event = match reader.next_frame().await {
            Ok(Some(frame)) => ClientEvent::Frame(frame),
            Ok(None) => ClientEvent::Eof,
            // An oversized or bogus length is the client breaking the
            // protocol and is named as such; anything else is a socket
            // that died, which is just an EOF with extra steps.
            Err(roost_ipc::Error::DataFrameTooLarge) => ClientEvent::Fatal(format!(
                "a client frame claimed more than the {MAX_DATA_FRAME_BYTES}-byte cap"
            )),
            Err(_) => ClientEvent::Eof,
        };
        let done = matches!(event, ClientEvent::Eof | ClientEvent::Fatal(_));
        if tx.send(event).await.is_err() || done {
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
