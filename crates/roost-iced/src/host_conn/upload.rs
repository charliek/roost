//! The upload lane (plan 047 §3.3): one dispatcher per incarnation, one
//! fresh connection per file.
//!
//! Uploads cannot ride the control leg. That loop awaits every op inline
//! ([`super::task::run_intent`]), so while one control op is in flight it
//! cannot even *receive* an upload, let alone run one — and a 14 MiB
//! frame timing out there would drop the host. So the lane is its own
//! task with its own bounded channel, its own connections and its own
//! budget: a timeout or a dial failure fails that upload alone, and the
//! host stays `Connected`. A takeover mid-upload arrives the same way,
//! as that upload's refusal; the main connection alone decides host
//! state.
//!
//! What ties the lane to the connection it belongs to is cancellation.
//! [`Uploads`] is a slot the UI holds and the connection task fills at
//! the `Connected` edge; the [`Lane`] guard it gets back empties the slot
//! and cancels the dispatcher the moment that incarnation ends — every
//! `ConnEnd` arm, an explicit reconnect, `HostConn::drop`, and the task's
//! future being dropped out from under it, because the guard's `Drop` is
//! what runs on all four. Nothing may answer after a new incarnation
//! exists, and no detached upload may outlive the app.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use roost_ipc::client::{IpcClient, ServerCode};
use roost_ipc::messages::{ops, SessionPutFileParams, SessionPutFileResult, MAX_PUT_FILE_BYTES};
use roost_ui_model::file_transfer::is_valid_put_file_name;
use tokio::sync::{mpsc, oneshot, Semaphore};
use tokio::task::JoinSet;

use super::queue::{self, HostOpError};
use super::task::{scale, Shutdown};

/// How many uploads may wait on one host before enqueuing fails.
///
/// Small on purpose: a gesture uploads its files one at a time (§3.3),
/// so anything past a handful is a caller that stopped reading its own
/// replies. The bound is the reason a wedged host costs a bounded amount
/// of memory, and a full queue is refused at the enqueue rather than
/// awaited — the enqueue happens on the main thread.
const QUEUE_DEPTH: usize = 8;

/// How many uploads run at once — per host, and across the process.
///
/// N hosts must not mean N 14 MiB frames and N `ssh` execs at once, so
/// the same number bounds both: [`IN_FLIGHT_UPLOADS`] across every host,
/// and the dispatcher's own gate on how many subtasks it spawns. Without
/// the second one the channel would drain as fast as requests arrive,
/// every request would sit in a subtask parked on the semaphore, and
/// [`QUEUE_DEPTH`] would bound nothing at all.
const IN_FLIGHT: usize = 2;

/// The process-wide in-flight bound. See [`IN_FLIGHT`].
static IN_FLIGHT_UPLOADS: Semaphore = Semaphore::const_new(IN_FLIGHT);

/// The fixed part of one upload's budget: enough for a dial over a
/// warm ControlMaster plus the session's own write.
const BUDGET_BASE: Duration = Duration::from_secs(30);

/// What each MiB of *encoded* payload adds — 100 KB/s plus half again.
const BUDGET_PER_ENCODED_MIB: Duration = Duration::from_secs(20);

/// How long one upload gets for its dial and its op together.
///
/// Injectable ([`Uploads::with_budget`]) so a test need not wait out a
/// real one.
pub(crate) type Budget = fn(u64) -> Duration;

/// `(30 s + 20 s × encoded MiB) × scale()`.
///
/// Sized on the **encoded** bytes because that is what crosses the wire:
/// base64 is `div_ceil(3) * 4`, so a 10 MiB file is 13.4 MiB and gets
/// ~298 s. The control leg's budget would not do — it is sized for a
/// `tab.list`, and a slow link is not a fault here.
pub(crate) fn budget(raw: u64) -> Duration {
    let encoded = raw.div_ceil(3) * 4;
    let mib = encoded as f64 / (1024.0 * 1024.0);
    (BUDGET_BASE + BUDGET_PER_ENCODED_MIB.mul_f64(mib)).mul_f64(scale())
}

/// Where one upload's bytes come from.
///
/// The clipboard's PNG is already bytes and never touches the client's
/// disk on a host tab; a dropped file is read at upload time rather than
/// trusted from the inspection that planned it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UploadSource {
    Path(PathBuf),
    Bytes(Vec<u8>),
}

/// What an upload answers with.
pub(crate) type UploadResult = Result<SessionPutFileResult, HostOpError>;

/// One queued upload, and where its answer goes.
///
/// The reply sender is carried to the dispatcher and lives there for the
/// upload's whole life — never in the subtask. That is what makes
/// "answered exactly once" structural: an aborted subtask cannot have
/// answered, and the dispatcher holds the only sender there is.
struct Upload {
    name: String,
    source: UploadSource,
    reply: oneshot::Sender<UploadResult>,
}

/// The UI-side handle on a host's upload lane.
///
/// Cloneable and cheap, like [`super::queue::HostOps`] — which holds
/// one. The slot is empty except while an incarnation is `Connected`, so
/// an upload asked of a host that is not connected is refused here
/// rather than queued behind a connect.
#[derive(Debug, Clone)]
pub(crate) struct Uploads {
    lane: Arc<Mutex<Option<mpsc::Sender<Upload>>>>,
    /// Why the lane is closed, when it is closed because another client
    /// took the foreground rather than because the host went away (plan
    /// 057 §3.5).
    ///
    /// An empty lane cannot tell the two apart on its own, and the two
    /// are not the same sentence: one says the session is gone, the other
    /// says it is right there and somebody else is driving it.
    deposed: Arc<Mutex<Option<Deposed>>>,
    budget: Budget,
}

impl Default for Uploads {
    fn default() -> Self {
        Uploads::with_budget(budget)
    }
}

impl Uploads {
    pub(crate) fn with_budget(budget: Budget) -> Uploads {
        Uploads {
            lane: Arc::new(Mutex::new(None)),
            deposed: Arc::new(Mutex::new(None)),
            budget,
        }
    }

    /// Another client is the foreground: an upload asked from here is
    /// refused as [`HostOpError::NotForeground`] until the foreground
    /// comes back.
    ///
    /// The task calls it on the deposition edge, *after* the lane guard
    /// has closed the lane — the guard is what stops a dispatcher dialing
    /// `session.put_file` with a lease this client no longer holds, and
    /// this only decides which sentence the refusal carries.
    pub(crate) fn deposed(&self, label: &str, taken_by: Option<&str>) {
        *self.locked_deposed() = Some(Deposed {
            label: label.to_string(),
            taken_by: taken_by.map(str::to_string),
        });
    }

    /// This client drives again, or has stopped watching altogether:
    /// whatever the lane answers from here on, it is not "somebody else
    /// is driving".
    pub(crate) fn foreground(&self) {
        *self.locked_deposed() = None;
    }

    /// Open the lane for one incarnation and start its dispatcher.
    ///
    /// The returned guard is the incarnation's lifetime: dropping it
    /// empties the slot and cancels every upload, in flight or queued.
    pub(crate) fn open(&self, socket: PathBuf, lease: String) -> Lane {
        let (tx, rx) = mpsc::channel(QUEUE_DEPTH);
        let cancel = Arc::new(Shutdown::default());
        // A lane exists only where the lease does, so opening one is the
        // one edge that proves this client is the foreground again.
        self.foreground();
        *self.locked() = Some(tx.clone());
        tokio::spawn(dispatch(
            rx,
            Wire {
                socket,
                lease,
                budget: self.budget,
            },
            Arc::clone(&cancel),
        ));
        Lane {
            cancel,
            uploads: self.clone(),
            sender: tx,
        }
    }

    /// Hand one upload to the dispatcher, and hand back what will carry
    /// its answer.
    ///
    /// Never blocks and never awaits: the caller is the main thread. A
    /// receiver that comes back is one the dispatcher will answer — the
    /// caller hearing nothing at all is the one outcome this lane does
    /// not have.
    pub(crate) fn enqueue(
        &self,
        name: String,
        source: UploadSource,
    ) -> Result<oneshot::Receiver<UploadResult>, HostOpError> {
        let Some(lane) = self.locked().clone() else {
            // No dispatcher means no lease. Which of the two reasons for
            // that it is decides the sentence: a host that went away, or
            // one that is right there under another client's foreground.
            return Err(self.no_lane());
        };
        let (tx, rx) = oneshot::channel();
        let upload = Upload {
            name,
            source,
            reply: tx,
        };
        match lane.try_send(upload) {
            Ok(()) => Ok(rx),
            Err(mpsc::error::TrySendError::Full(upload)) => {
                tracing::warn!(name = %upload.name, "the host upload queue is full");
                Err(HostOpError::Unavailable)
            }
            // The dispatcher is winding down; a new one belongs to a
            // different incarnation, which this upload was not asked of.
            Err(mpsc::error::TrySendError::Closed(_)) => Err(HostOpError::Disconnected),
        }
    }

    /// Empty the slot, but only if it still holds `mine`.
    ///
    /// A guard can outlive the install of its successor — the connection
    /// loop's next attempt runs whether or not the old dispatcher has
    /// finished unwinding — so an unconditional clear would close a lane
    /// that belongs to the incarnation after this one.
    fn close(&self, mine: &mpsc::Sender<Upload>) {
        let mut lane = self.locked();
        if lane.as_ref().is_some_and(|held| held.same_channel(mine)) {
            *lane = None;
        }
    }

    /// Why there is no lane to enqueue onto.
    fn no_lane(&self) -> HostOpError {
        match self.locked_deposed().clone() {
            Some(Deposed { label, taken_by }) => HostOpError::NotForeground { label, taken_by },
            None => HostOpError::Disconnected,
        }
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, Option<mpsc::Sender<Upload>>> {
        self.lane
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn locked_deposed(&self) -> std::sync::MutexGuard<'_, Option<Deposed>> {
        self.deposed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Who holds the foreground this lane's host is not holding.
#[derive(Debug, Clone)]
struct Deposed {
    label: String,
    taken_by: Option<String>,
}

/// One incarnation's hold on its lane. See [`Uploads::open`].
pub(crate) struct Lane {
    cancel: Arc<Shutdown>,
    uploads: Uploads,
    sender: mpsc::Sender<Upload>,
}

impl Drop for Lane {
    fn drop(&mut self) {
        // Both halves matter, and in this order: closing the slot means
        // no further upload can be admitted onto a connection that is
        // going away, and the signal is what makes the dispatcher answer
        // the ones already there instead of being dropped with their
        // reply channels.
        self.uploads.close(&self.sender);
        self.cancel.request();
    }
}

/// Everything one upload needs to reach the session, fixed for the
/// incarnation.
#[derive(Clone)]
struct Wire {
    socket: PathBuf,
    lease: String,
    budget: Budget,
}

/// The dispatcher: admit uploads, run them, answer them, and answer
/// everything left when the incarnation ends.
async fn dispatch(mut rx: mpsc::Receiver<Upload>, wire: Wire, cancel: Arc<Shutdown>) {
    let mut running: JoinSet<UploadResult> = JoinSet::new();
    let mut waiting: HashMap<tokio::task::Id, oneshot::Sender<UploadResult>> = HashMap::new();

    loop {
        tokio::select! {
            biased;
            () = cancel.requested() => break,
            joined = running.join_next_with_id(), if !running.is_empty() => {
                match joined {
                    Some(Ok((id, outcome))) => answer(&mut waiting, id, outcome),
                    // A panicked subtask still owes its caller an
                    // answer; an abort cannot land here, because the
                    // only aborts are the ones below.
                    Some(Err(error)) => answer(
                        &mut waiting,
                        error.id(),
                        Err(HostOpError::Local(format!("the upload failed: {error}"))),
                    ),
                    None => {}
                }
            }
            upload = rx.recv(), if running.len() < IN_FLIGHT => {
                // Every sender is gone: the guard was dropped, so this
                // incarnation is over.
                let Some(upload) = upload else { break };
                let handle = running.spawn(put_file(wire.clone(), upload.name, upload.source));
                waiting.insert(handle.id(), upload.reply);
            }
        }
    }

    // The incarnation is over. Abort what is running — each subtask's
    // connection closes with it, through the ordinary drop — and answer
    // everything, in flight or still queued, exactly once. The senders
    // are all here and nowhere else, so there is no second answer to
    // race and none to lose.
    running.abort_all();
    rx.close();
    for reply in waiting.into_values() {
        let _ = reply.send(Err(HostOpError::Disconnected));
    }
    while let Ok(upload) = rx.try_recv() {
        let _ = upload.reply.send(Err(HostOpError::Disconnected));
    }
}

fn answer(
    waiting: &mut HashMap<tokio::task::Id, oneshot::Sender<UploadResult>>,
    id: tokio::task::Id,
    outcome: UploadResult,
) {
    if let Some(reply) = waiting.remove(&id) {
        // A dropped receiver means the caller lost interest, which is an
        // ordinary outcome rather than a fault.
        let _ = reply.send(outcome);
    }
}

/// One upload, end to end: read, encode, dial, send, validate.
async fn put_file(wire: Wire, name: String, source: UploadSource) -> UploadResult {
    let _permit = IN_FLIGHT_UPLOADS
        .acquire()
        .await
        .expect("the in-flight semaphore is never closed");

    // The read and the base64 encode both go to the blocking pool: a
    // 10 MiB file on an NFS `$HOME` must not park a runtime worker, and
    // neither may ever run where the winit thread could see it.
    let encoding = {
        let (name, lease) = (name.clone(), wire.lease.clone());
        tokio::task::spawn_blocking(move || encode(lease, name, source))
    };
    let (bytes, params) = encoding
        .await
        .map_err(|error| HostOpError::Local(format!("reading {name} failed: {error}")))??;

    let budget = (wire.budget)(bytes);
    let raw = tokio::time::timeout(budget, send(&wire.socket, params))
        .await
        .map_err(|_elapsed| {
            HostOpError::Transport(format!("{} timed out", ops::SESSION_PUT_FILE))
        })??;
    let result: SessionPutFileResult = serde_json::from_value(raw).map_err(|error| {
        HostOpError::Local(format!("{} did not decode: {error}", ops::SESSION_PUT_FILE))
    })?;
    validate(&name, bytes, result)
}

/// Dial the host's socket afresh and send the one op.
///
/// A connection per upload, the `tab.attach` precedent: the control
/// client is serial and busy, and `require_lease` registers this
/// connection under the same lease, so a takeover or a `session.stop`
/// closes it too — as a per-upload failure.
async fn send(socket: &Path, params: serde_json::Value) -> Result<serde_json::Value, HostOpError> {
    let mut client = IpcClient::connect(socket)
        .await
        .map_err(|error| HostOpError::Transport(error.to_string()))?;
    client
        .call_raw(ops::SESSION_PUT_FILE, params)
        .await
        // The fault half is deliberately dropped: what a refusal means
        // for *this upload* is all this lane decides, and the control
        // connection alone decides what the host's state is.
        .map_err(|error| queue::classify(&error).1)
}

/// Read the source and build the frame's params. Blocking.
fn encode(
    lease: String,
    name: String,
    source: UploadSource,
) -> Result<(u64, serde_json::Value), HostOpError> {
    let data = match source {
        UploadSource::Path(path) => read_capped(&path, &name)?,
        UploadSource::Bytes(bytes) => bytes,
    };
    let bytes = data.len() as u64;
    if bytes > MAX_PUT_FILE_BYTES {
        return Err(too_large(&name, bytes));
    }
    let params = serde_json::to_value(SessionPutFileParams { lease, name, data })
        .map_err(|error| HostOpError::Local(format!("encoding the upload failed: {error}")))?;
    Ok((bytes, params))
}

/// Open the file, `fstat` the handle, and read at most one byte past the
/// cap.
///
/// The bytes that go on the wire are the ones the handle holds *now*:
/// the plan inspected this path earlier (`file_transfer::Candidate`), and
/// a file that grew since then, or that was swapped for a FIFO whose
/// `len()` lies, is caught here — before anything is dialed — rather
/// than on the wire as a `too-large` from the far side.
fn read_capped(path: &Path, name: &str) -> Result<Vec<u8>, HostOpError> {
    let local = |error: std::io::Error| HostOpError::Local(format!("{name}: {error}"));
    let file = std::fs::File::open(path).map_err(local)?;
    let stat = file.metadata().map_err(local)?;
    if !stat.is_file() {
        return Err(HostOpError::Local(format!("{name} is not a regular file")));
    }
    if stat.len() > MAX_PUT_FILE_BYTES {
        return Err(too_large(name, stat.len()));
    }
    let mut data = Vec::with_capacity(stat.len() as usize);
    file.take(MAX_PUT_FILE_BYTES + 1)
        .read_to_end(&mut data)
        .map_err(local)?;
    let read = data.len() as u64;
    if read > MAX_PUT_FILE_BYTES {
        return Err(too_large(name, read));
    }
    Ok(data)
}

/// The client's own copy of the server's `too-large`.
///
/// Spelled as the session's refusal rather than as a local error on
/// purpose: whether the cap was caught here or on the far side is not a
/// distinction the status line or `tab.send_file`'s error precedence
/// should have to make.
fn too_large(name: &str, bytes: u64) -> HostOpError {
    HostOpError::Rejected {
        code: ServerCode::TooLarge,
        message: format!(
            "{name} is {bytes} bytes, over the {} MiB limit",
            MAX_PUT_FILE_BYTES / (1024 * 1024)
        ),
    }
}

/// Re-check the session's answer before anybody pastes it (§3.1).
///
/// The session guarantees a paste-safe path, and this is the client
/// refusing to take that on trust: a malformed or hostile reply must
/// never become typed input. Absolute, every byte in `[A-Za-z0-9._/-]`,
/// the final component exactly the `name` that was sent — and by the
/// name rule both ends share — and the byte count exactly what was sent.
fn validate(name: &str, sent: u64, result: SessionPutFileResult) -> UploadResult {
    let refuse = |what: &str| {
        Err(HostOpError::Local(format!(
            "{} answered with {what}",
            ops::SESSION_PUT_FILE
        )))
    };
    if !result.path.starts_with('/') {
        return refuse("a path that is not absolute");
    }
    if !result
        .path
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/'))
    {
        return refuse("a path no agent could unquote");
    }
    let landed = result.path.rsplit('/').next().unwrap_or_default();
    if landed != name || !is_valid_put_file_name(landed) {
        return refuse("a different file name");
    }
    if result.bytes != sent {
        return refuse("a byte count that is not what was sent");
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(path: &str, bytes: u64) -> SessionPutFileResult {
        SessionPutFileResult {
            path: path.into(),
            bytes,
        }
    }

    fn local(outcome: UploadResult) -> String {
        match outcome {
            Err(HostOpError::Local(message)) => message,
            other => panic!("expected a refusal by this client, got {other:?}"),
        }
    }

    /// The happy path the four refusals below are measured against.
    #[test]
    fn a_well_formed_reply_is_taken_as_it_stands() {
        let reply = result("/home/c/.cache/roost-session/files/4b9d1e7f/shot.png", 12);
        assert_eq!(validate("shot.png", 12, reply.clone()), Ok(reply));
    }

    /// §3.1's re-check, clause by clause. A session is trusted to write
    /// the file; it is not trusted to name what the user's shell will
    /// then be typed.
    #[test]
    fn a_hostile_reply_is_never_pasted() {
        // The final component is not the name that was sent — the one
        // that would paste a path to somebody else's file.
        assert!(local(validate(
            "shot.png",
            12,
            result("/files/4b9d/other.png", 12)
        ))
        .contains("a different file name"));

        // Bytes outside the grammar. Each of these is a different way to
        // turn one pasted path into two shell words, or into a line the
        // agent never sees.
        for hostile in [
            "/files/4b9d/shot.png ; rm -rf ~",
            "/files/$(id)/shot.png",
            "/files/4b9d\n/shot.png",
        ] {
            assert!(
                local(validate("shot.png", 12, result(hostile, 12)))
                    .contains("a path no agent could unquote"),
                "{hostile}"
            );
        }

        // Relative: nothing says which directory it would resolve in.
        assert!(
            local(validate("shot.png", 12, result("files/4b9d/shot.png", 12)))
                .contains("not absolute")
        );

        // The count is the one thing the client can check about the
        // bytes themselves.
        assert!(
            local(validate("shot.png", 12, result("/files/4b9d/shot.png", 11)))
                .contains("byte count")
        );

        // A name that never satisfied the shared rule cannot come back
        // "matching" it either.
        assert!(local(validate("-rf", 12, result("/files/4b9d/-rf", 12)))
            .contains("a different file name"));
    }

    /// The budget is sized on the bytes that cross the wire, not the
    /// bytes on disk: base64 is a third bigger, and a link that is fine
    /// for 10 MiB is not fine for 13.4.
    #[test]
    fn the_budget_grows_with_the_encoded_payload() {
        assert_eq!(budget(0), BUDGET_BASE.mul_f64(scale()));

        // 10 MiB raw → 13 981 016 encoded bytes → 13.33 MiB.
        let ten_mib = budget(MAX_PUT_FILE_BYTES);
        assert!(
            ten_mib >= Duration::from_secs(290).mul_f64(scale())
                && ten_mib <= Duration::from_secs(300).mul_f64(scale()),
            "{ten_mib:?}"
        );
        assert!(ten_mib > budget(MAX_PUT_FILE_BYTES / 2));
    }

    /// The queue is bounded and refuses at the enqueue: it is called on
    /// the main thread, so awaiting a full queue would stall the frame.
    #[tokio::test]
    async fn a_full_queue_refuses_rather_than_blocking() {
        let uploads = Uploads::default();
        let (tx, mut rx) = mpsc::channel(QUEUE_DEPTH);
        *uploads.locked() = Some(tx);

        // Held, not dropped: a caller waiting on its receiver is what
        // fills a real queue.
        let _waiting: Vec<_> = (0..QUEUE_DEPTH)
            .map(|nth| {
                uploads
                    .enqueue(format!("{nth}.png"), UploadSource::Bytes(vec![1]))
                    .expect("the queue takes its depth")
            })
            .collect();
        assert_eq!(
            uploads
                .enqueue("overflow.png".into(), UploadSource::Bytes(vec![1]))
                .err(),
            Some(HostOpError::Unavailable),
            "the ninth is refused, not awaited"
        );

        // And nothing that *was* taken is disturbed by the refusal.
        for nth in 0..QUEUE_DEPTH {
            assert_eq!(rx.try_recv().expect("queued").name, format!("{nth}.png"));
        }
    }

    /// No dispatcher means no incarnation. An upload asked of a host
    /// that is not connected is refused here, rather than queued behind
    /// a connect that may never happen.
    #[tokio::test]
    async fn a_closed_lane_refuses_immediately() {
        let uploads = Uploads::default();
        assert_eq!(
            uploads
                .enqueue("shot.png".into(), UploadSource::Bytes(vec![1]))
                .err(),
            Some(HostOpError::Disconnected)
        );
    }

    /// The two reasons a lane can be closed are not the same sentence
    /// (plan 057 §3.5): a deposed client's host is *right there* and
    /// still serving its grid, and telling the user it disconnected
    /// would be a lie about a session they can watch updating.
    #[tokio::test]
    async fn an_upload_while_deposed_is_refused_not_foreground_not_disconnected() {
        let uploads = Uploads::default();
        uploads.deposed("workbox", Some("a phone"));

        let refusal = uploads
            .enqueue("shot.png".into(), UploadSource::Bytes(vec![1]))
            .expect_err("a deposed client has no lane");
        assert_eq!(
            refusal,
            HostOpError::NotForeground {
                label: "workbox".into(),
                taken_by: Some("a phone".into()),
            }
        );
        assert_eq!(
            refusal.to_string(),
            "workbox is driven by a phone; take the foreground to upload"
        );

        // A takeover this client only inferred names nobody, and the
        // sentence still has to read.
        uploads.deposed("workbox", None);
        assert_eq!(
            uploads
                .enqueue("shot.png".into(), UploadSource::Bytes(vec![1]))
                .expect_err("still no lane")
                .to_string(),
            "workbox is driven by another client; take the foreground to upload"
        );

        // Opening a lane is what proves the foreground came back, so a
        // stale label can never outlive it.
        let _lane = uploads.open(PathBuf::from("/nonexistent.sock"), "the-lease".into());
        drop(_lane);
        assert_eq!(
            uploads
                .enqueue("shot.png".into(), UploadSource::Bytes(vec![1]))
                .err(),
            Some(HostOpError::Disconnected)
        );
    }

    /// A guard that outlives the install of its successor must not close
    /// the *next* incarnation's lane.
    #[tokio::test]
    async fn closing_a_stale_lane_leaves_the_live_one_alone() {
        let uploads = Uploads::default();
        let (stale, _stale_rx) = mpsc::channel(QUEUE_DEPTH);
        let (live, mut live_rx) = mpsc::channel(QUEUE_DEPTH);
        *uploads.locked() = Some(live);

        uploads.close(&stale);

        uploads
            .enqueue("shot.png".into(), UploadSource::Bytes(vec![1]))
            .expect("the live lane is still open");
        assert_eq!(live_rx.try_recv().expect("queued").name, "shot.png");
    }
}
