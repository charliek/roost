//! `TerminalHandle`: one attached tab as Dart holds it — the owner
//! thread's terminal, the attach stream feeding it, and the task between
//! them.
//!
//! The terminal's outputs come back on the owner thread and are routed
//! here: input and `hydrated` to the current stream, events to Dart's
//! sink (buffered until Dart provides one), and a re-attach request by
//! replacing the stream with a fresh one at the tab's current geometry.
//! A replacement is never a deliberate switch, so it never takes the
//! size.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};

use roost_client::attach::{AttachEvent, AttachStream};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::app::{AppError, Inner, SharedSink, Sink};
use crate::keys::KeyInput;
use crate::live::Live;
use crate::owner::{TabOutput, TerminalEvent, TerminalId, TerminalOwner};
use crate::terminal::Geometry;

/// Events kept for a handle whose sink Dart has not set yet. The owner
/// sends one `FrameReady` until a pull, so only a status storm could
/// fill this; the oldest go first.
const EVENT_BUFFER_CAP: usize = 256;

enum Events {
    Buffered(Vec<TerminalEvent>),
    Sink(SharedSink<TerminalEvent>),
}

pub(crate) struct Attachment {
    id: OnceLock<TerminalId>,
    pub(crate) host_id: String,
    pub(crate) tab_id: i64,
    inner: Weak<Inner>,
    owner: Arc<TerminalOwner>,
    stream: Mutex<Option<AttachStream>>,
    /// The tab's grid as this client last asked for it: what a
    /// replacement stream's handshake states.
    geometry: Mutex<Geometry>,
    events: Mutex<Events>,
    /// No stream will feed the terminal again: paused, closed, or the
    /// app is going away.
    ended: AtomicBool,
    closed: AtomicBool,
    _live: Live,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Attachment {
    pub(crate) fn id(&self) -> TerminalId {
        *self
            .id
            .get()
            .expect("the id is set as the attachment opens")
    }

    pub(crate) fn streaming(&self) -> bool {
        lock(&self.stream).is_some()
    }

    fn emit(&self, event: TerminalEvent) {
        let mut events = lock(&self.events);
        match &mut *events {
            Events::Buffered(buffer) => {
                if buffer.len() >= EVENT_BUFFER_CAP {
                    buffer.remove(0);
                }
                buffer.push(event);
            }
            Events::Sink(sink) => sink(event),
        }
    }

    fn with_stream(&self, f: impl FnOnce(&AttachStream)) {
        if let Some(stream) = lock(&self.stream).as_ref() {
            f(stream);
        }
    }

    /// Drop the stream (its task writes `DETACH`) without reporting it.
    pub(crate) fn detach(&self) {
        self.ended.store(true, Ordering::SeqCst);
        let stream = lock(&self.stream).take();
        drop(stream);
    }

    /// Detach on purpose and say so: the terminal reports `Ended` with
    /// `reason`, and whatever the stream still had in flight is dropped.
    pub(crate) fn end(&self, reason: &str) {
        if self.ended.swap(true, Ordering::SeqCst) {
            return;
        }
        let stream = lock(&self.stream).take();
        drop(stream);
        self.owner.end_stream(self.id(), reason.to_string());
    }

    /// No stream replaces this one from here on, but the one there stays
    /// until the owner thread has worked through what was queued before
    /// the close: a key typed just before it still reaches the tab.
    fn close(self: &Arc<Self>) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.ended.store(true, Ordering::SeqCst);
        let closing = Arc::clone(self);
        self.owner.after(Box::new(move || closing.detach()));
        self.owner.close(self.id());
        if let Some(inner) = self.inner.upgrade() {
            inner.forget_attachment(self.id());
        }
    }
}

/// Runs on the owner thread, in the terminal's output order.
fn route(attachment: &Arc<Attachment>, output: TabOutput) {
    match output {
        TabOutput::Event(event) => attachment.emit(event),
        TabOutput::Input { bytes, take } => attachment.with_stream(|s| s.input(bytes, take)),
        TabOutput::Hydrated => attachment.with_stream(AttachStream::hydrated),
        TabOutput::Reattach { reason, stream } => replace_stream(attachment, stream, &reason),
    }
}

/// The terminal is off the stream's resume point: a stream told only to
/// forget it would keep its connection, so drop it and dial afresh.
fn replace_stream(attachment: &Arc<Attachment>, number: u64, reason: &str) {
    let Some(inner) = attachment.inner.upgrade() else {
        return;
    };
    if attachment.ended.load(Ordering::SeqCst) || inner.closed.load(Ordering::SeqCst) {
        return;
    }
    tracing::debug!(tab = attachment.tab_id, %reason, "re-attaching for a fresh snapshot");
    let geometry = *lock(&attachment.geometry);
    let fresh = inner.spawn_stream(&attachment.host_id, attachment.tab_id, geometry, false);
    let (stream, events) = match fresh {
        Ok(fresh) => fresh,
        Err(error) => {
            let old = lock(&attachment.stream).take();
            drop(old);
            attachment
                .owner
                .end_stream(attachment.id(), error.to_string());
            return;
        }
    };
    let old = {
        let mut current = lock(&attachment.stream);
        // Checked under the lock `close` takes the stream through, so a
        // close racing this never leaves a stream behind it.
        if attachment.ended.load(Ordering::SeqCst) {
            Some(stream)
        } else {
            let old = current.replace(stream);
            inner.rt.spawn(pump(
                Arc::clone(&attachment.owner),
                attachment.id(),
                number,
                events,
                Live::new(&inner.counters.attach_pumps),
            ));
            old
        }
    };
    drop(old);
}

async fn pump(
    owner: Arc<TerminalOwner>,
    id: TerminalId,
    stream: u64,
    mut events: UnboundedReceiver<AttachEvent>,
    _live: Live,
) {
    while let Some(event) = events.recv().await {
        owner.apply(id, stream, event);
    }
}

pub(crate) fn open(
    inner: &Arc<Inner>,
    host_id: String,
    tab_id: i64,
    geometry: Geometry,
    take: bool,
) -> Result<Arc<Attachment>, AppError> {
    let (stream, events) = inner.spawn_stream(&host_id, tab_id, geometry, take)?;
    let attachment = Arc::new(Attachment {
        id: OnceLock::new(),
        host_id,
        tab_id,
        inner: Arc::downgrade(inner),
        owner: Arc::clone(&inner.owner),
        stream: Mutex::new(Some(stream)),
        geometry: Mutex::new(geometry),
        events: Mutex::new(Events::Buffered(Vec::new())),
        ended: AtomicBool::new(false),
        closed: AtomicBool::new(false),
        _live: Live::new(&inner.counters.attachments),
    });
    let routed = Arc::clone(&attachment);
    let id = inner
        .owner
        .open(Box::new(move |output| route(&routed, output)));
    let _ = attachment.id.set(id);
    // After `open`, so the owner has the tab before its first event.
    inner.rt.spawn(pump(
        Arc::clone(&inner.owner),
        id,
        0,
        events,
        Live::new(&inner.counters.attach_pumps),
    ));
    Ok(attachment)
}

/// One attached tab (plan 089 C5). Dropping it closes it.
pub struct TerminalHandle {
    attachment: Arc<Attachment>,
}

impl TerminalHandle {
    pub(crate) fn new(attachment: Arc<Attachment>) -> Self {
        Self { attachment }
    }

    /// The terminal's events, including any sent before this was set.
    /// A later sink replaces this one.
    ///
    /// The sink runs on the terminal thread: it must hand the event off
    /// (as a Dart `StreamSink` does), not act on it in place. A frame
    /// pulled from inside it — `take_frame`, `viewport_text` — comes back
    /// empty, since answering would mean the thread waiting on itself.
    pub fn events(&self, sink: Sink<TerminalEvent>) {
        let sink: SharedSink<TerminalEvent> = Arc::from(sink);
        let mut events = lock(&self.attachment.events);
        let buffered = match std::mem::replace(&mut *events, Events::Sink(Arc::clone(&sink))) {
            Events::Buffered(buffered) => buffered,
            Events::Sink(_) => Vec::new(),
        };
        for event in buffered {
            sink(event);
        }
    }

    /// What changed since the last pull (C6); empty when nothing did.
    pub fn take_frame(&self) -> Vec<u8> {
        self.attachment
            .owner
            .take_frame(self.attachment.id(), false)
    }

    /// Every row, for a painter that lost its row cache.
    pub fn take_full_frame(&self) -> Vec<u8> {
        self.attachment.owner.take_frame(self.attachment.id(), true)
    }

    /// A hardware or extra-keys press; takes the size before it lands.
    pub fn key(&self, key: KeyInput) {
        self.attachment.owner.key(self.attachment.id(), key);
    }

    /// Committed soft-keyboard text; takes the size before it lands.
    pub fn text(&self, text: String) {
        self.attachment.owner.text(self.attachment.id(), text);
    }

    /// Bracketed when the program asked for it; takes the size first.
    pub fn paste(&self, text: String) {
        self.attachment.owner.paste(self.attachment.id(), text);
    }

    /// `rows` positive toward older history: the local viewport, or —
    /// when the program reads the wheel — reports aimed at (`col`, `row`),
    /// which take the size after they land.
    pub fn scroll(&self, rows: i32, col: u16, row: u16) {
        self.attachment
            .owner
            .scroll(self.attachment.id(), rows, col, row);
    }

    /// This client's grid. The terminal itself follows the tab's grid as
    /// the session reports it, not this. `focused`: the user resized
    /// while looking at the tab, which takes its size.
    pub fn resize(&self, geometry: Geometry, focused: bool) {
        *lock(&self.attachment.geometry) = geometry;
        self.attachment.with_stream(|stream| {
            stream.resize(
                geometry.cols,
                geometry.rows,
                geometry.cell_w_px,
                geometry.cell_h_px,
                focused,
            );
        });
    }

    /// Take the tab's size at this client's grid.
    pub fn take_size(&self) {
        self.attachment.with_stream(AttachStream::take);
    }

    /// Detach and drop the terminal. Idempotent.
    pub fn close(&self) {
        self.attachment.close();
    }

    /// The drawn viewport as text, one trimmed line per row.
    pub fn viewport_text(&self) -> Vec<String> {
        self.attachment.owner.viewport_text(self.attachment.id())
    }
}

impl Drop for TerminalHandle {
    fn drop(&mut self) {
        self.close();
    }
}
