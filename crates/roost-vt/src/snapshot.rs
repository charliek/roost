//! Safe wrapper around `ghostty_snapshot_*` — the terminal snapshot
//! format that host-sessions (HS-1) uses as its attach payload.
//!
//! [`Terminal::snapshot`] serializes a terminal to a byte vector through
//! `ghostty_snapshot_encode`; [`SnapshotDecoder`] restores one, either
//! from a complete buffer ([`SnapshotDecoder::decode_bytes`]) or
//! progressively as transport bytes arrive.
//!
//! # Why the decoder owns the buffering
//!
//! libghostty's decoder pulls through a synchronous `GhosttyReader` and
//! treats a zero-byte read as permanent EOF (`io.h:28-53`), so it cannot
//! be driven directly off a socket that has not delivered the whole
//! snapshot yet. This wrapper therefore buffers the stream itself and
//! parses **only** the framing the format documents — the ten-byte
//! envelope and each record's `tag | payload_len | crc` header
//! (`snapshot.h:53-118`) — to find record boundaries. It calls
//! `decoder_ready` only once the complete prefix through the READY
//! record is buffered and `decoder_next` only once the next PAGE (or
//! FINISH) record is, so every read the C decoder issues completes from
//! memory. Every *semantic* judgment — CRC checks, record order, version
//! acceptance, continuation validation — stays inside libghostty; the
//! scanner never validates, it only measures.
//!
//! # No Swift twin
//!
//! Every other libghostty capability roost uses is mirrored on the Mac
//! side, but the Swift app calls `ghostty_terminal_new` and friends
//! directly and never consumes `roost-vt`. Host sessions ship with the
//! iced UI as their only client (roadmap D1/D5), so there is
//! deliberately no Swift counterpart to this API and `mac/` stays
//! untouched. Revisit only if the Mac UI becomes a host-sessions client.

use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::os::raw::c_void;
use std::ptr;

use crate::sys;
use crate::terminal::{ActiveScreen, Terminal};
use crate::{Error, Result};

/// Userdata behind [`snapshot_writer_trampoline`]. `alloc_failed` keeps
/// an allocation failure distinguishable from a libghostty-side error:
/// the C API only sees `false` from the writer and reports it as
/// `GHOSTTY_IO_ERROR`, which would otherwise be indistinguishable from a
/// genuine encode fault.
struct SnapshotWriter {
    buf: Vec<u8>,
    alloc_failed: bool,
}

/// Trampoline installed as the `GhosttyWriter` callback for
/// `ghostty_snapshot_encode`.
///
/// Hygiene contract (all load-bearing for FFI soundness):
/// * Guards null `userdata`/`data`; a null userdata cannot be honored,
///   so it reports the fatal-write error rather than silently dropping
///   bytes.
/// * `try_reserve` first, so the subsequent `extend_from_slice` cannot
///   allocate and therefore cannot abort on OOM — no unwind, no abort
///   across the `extern "C"` frame.
/// * Never calls a terminal API on the encoding handle
///   (`snapshot.h:259-284`).
unsafe extern "C" fn snapshot_writer_trampoline(
    userdata: *mut c_void,
    data: *const u8,
    len: usize,
) -> bool {
    if userdata.is_null() || data.is_null() {
        return false;
    }
    if len == 0 {
        return true;
    }
    // SAFETY: `userdata` is the `&raw mut SnapshotWriter` installed in
    // `Terminal::snapshot`, which owns the value for the whole duration
    // of the synchronous `ghostty_snapshot_encode` call and holds no
    // other borrow of it meanwhile.
    let writer = unsafe { &mut *(userdata as *mut SnapshotWriter) };
    if writer.buf.try_reserve(len).is_err() {
        writer.alloc_failed = true;
        return false;
    }
    // SAFETY: libghostty guarantees `data`/`len` describe a valid,
    // initialized byte range for the duration of this call.
    let bytes = unsafe { std::slice::from_raw_parts(data, len) };
    writer.buf.extend_from_slice(bytes);
    true
}

impl Terminal {
    /// Encode this terminal to a snapshot byte stream: the 10-byte
    /// `"GHOSTSNP"` + u16-version envelope followed by CRC32C-checked
    /// records (`snapshot.h:53-118`).
    ///
    /// The `&self` borrow supplies the API's no-concurrent-mutation
    /// precondition.
    ///
    /// Encoding a terminal whose VT parser or UTF-8 decoder is *not* at
    /// ground requires that continuation tracking was enabled before the
    /// input which left it unfinished; otherwise this returns
    /// [`Error::InvalidValue`]. See
    /// [`TerminalOptions::continuation_max_bytes`](crate::TerminalOptions::continuation_max_bytes).
    pub fn snapshot(&self) -> Result<Vec<u8>> {
        let mut writer = SnapshotWriter {
            buf: Vec::new(),
            alloc_failed: false,
        };
        let sink = sys::GhosttyWriter {
            write: Some(snapshot_writer_trampoline),
            userdata: (&raw mut writer).cast(),
        };
        // SAFETY: handle non-null per the constructor; `sink` carries a
        // non-null callback and a pointer to `writer`, which outlives
        // this synchronous call.
        let rc = unsafe { sys::ghostty_snapshot_encode(self.handle(), sink) };
        // An allocation failure inside the trampoline surfaces as a
        // writer rejection; report the real cause instead.
        if writer.alloc_failed {
            return Err(Error::OutOfMemory);
        }
        Error::from_result(rc)?;
        Ok(writer.buf)
    }
}

// ============================================================================
// Decode
// ============================================================================

/// Fixed envelope: `"GHOSTSNP"` + a u16 LE version (`snapshot.h:53-63`).
/// The scanner only ever *skips* it — the version check belongs to the
/// decoder, and duplicating it here would hide a real format rejection
/// behind a wrapper one.
const ENVELOPE_LEN: usize = 10;

/// `tag u16 | payload_len u32 | crc u32` (`snapshot.h:69-77`).
const RECORD_HEADER_LEN: usize = 10;

/// Record tags, from the format's own framing table
/// (`snapshot/record.zig`'s `Tag`). The scanner reads them for boundary
/// purposes only: PAGE and FINISH are where `decoder_next` stops, READY
/// is where `decoder_ready` stops, and CONTINUATION is the one record
/// whose declared length the wrapper caps before any FFI call.
const TAG_PAGE: u16 = 3;
const TAG_READY: u16 = 5;
const TAG_FINISH: u16 = 6;
const TAG_CONTINUATION: u16 = 7;

/// Wrapper hard caps, generous but finite. A hostile or corrupt stream
/// must not be able to make roost buffer without bound before libghostty
/// ever sees a byte (architecture doc §5). HS-1 overrides these with its
/// own transport budget.
const DEFAULT_MAX_TOTAL_BYTES: usize = 512 * 1024 * 1024;
const DEFAULT_MAX_RECORD_BYTES: usize = 64 * 1024 * 1024;

/// Decode-side knobs, fixed at [`SnapshotDecoder::new`].
///
/// Options are taken at construction because libghostty refuses
/// `decoder_set` once decoding has started (`snapshot.h:120-126`);
/// making them unrepresentable after the fact removes the failure mode
/// rather than documenting it.
#[derive(Debug, Clone, Copy)]
pub struct SnapshotDecodeOptions {
    /// Largest non-ground continuation to accept, mapped to
    /// `OPT_MAX_CONTINUATION_BYTES`. `None` keeps libghostty's default
    /// (65 MiB at this pin); `Some(0)` accepts only snapshots whose VT
    /// parser was at ground.
    pub max_continuation_bytes: Option<usize>,
    /// `OPT_RETAIN_CONTINUATION`: apply the decoded continuation's
    /// tracking limit to the returned terminal so the continuation can
    /// be exported. Off by default; when on, callers that do not want
    /// ongoing tracking must call
    /// [`Terminal::set_continuation_max_bytes`]`(0)` after export and
    /// before writing post-snapshot input (`snapshot.h:143-161`).
    pub retain_continuation: bool,
    /// Cap on the total number of bytes fed into one decoder.
    pub max_total_bytes: usize,
    /// Cap on a single record's declared payload length.
    pub max_record_bytes: usize,
}

impl Default for SnapshotDecodeOptions {
    fn default() -> Self {
        Self {
            max_continuation_bytes: None,
            retain_continuation: false,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            max_record_bytes: DEFAULT_MAX_RECORD_BYTES,
        }
    }
}

/// Result of [`SnapshotDecoder::try_ready`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadyState {
    /// The prefix through READY is not buffered yet. Feed more bytes.
    NeedMoreBytes,
    /// The terminal is decoded and renderable;
    /// [`SnapshotDecoder::terminal`] now returns it.
    Ready,
}

/// Result of [`SnapshotDecoder::try_next`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryStep {
    /// The next history record is not fully buffered yet.
    NeedMoreBytes,
    /// One history page was consumed. `rows_prepended` is zero when the
    /// page was validated but could no longer be applied — e.g. after a
    /// resize (`snapshot.h:228-236`).
    Page {
        screen: ActiveScreen,
        rows_prepended: usize,
        pages_remaining_in_screen: u32,
    },
    /// FINISH validated. Idempotent: further calls also report
    /// `Finished`.
    Finished,
}

/// A fully decoded snapshot, handed out by [`SnapshotDecoder::finish`].
pub struct DecodedTerminal {
    pub terminal: Terminal,
    /// Offset of the first byte after FINISH — where trailing transport
    /// bytes begin (`snapshot.h:185-194`).
    pub source_offset: usize,
    /// Advisory complete history extent for the primary screen, cached
    /// at READY.
    pub history_rows_primary: u64,
    /// Same for the alternate screen; `None` when the snapshot declares
    /// no alternate screen.
    pub history_rows_alternate: Option<u64>,
}

impl std::fmt::Debug for DecodedTerminal {
    /// `Terminal` is an opaque FFI handle with no `Debug`, so the
    /// terminal is named rather than described.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodedTerminal")
            .field("source_offset", &self.source_offset)
            .field("history_rows_primary", &self.history_rows_primary)
            .field("history_rows_alternate", &self.history_rows_alternate)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Feeding,
    Ready,
    Finished,
    Poisoned,
}

/// Buffer + cursor behind [`snapshot_reader_trampoline`]. `fault`
/// records a wrapper-side failure so it can be reported as its real
/// cause instead of the `GHOSTTY_IO_ERROR` the C API would otherwise
/// show (same reasoning as the writer's `alloc_failed`).
struct ReaderState {
    buf: Vec<u8>,
    cursor: usize,
    fault: bool,
}

/// Trampoline installed as the `GhosttyReader` callback for
/// `ghostty_snapshot_decoder_new`.
///
/// Hygiene contract (all load-bearing for FFI soundness):
/// * Guards null `userdata`/`buffer`/`out_read`; without `userdata`
///   there is nowhere to record the fault, so it can only report the
///   fatal-read error.
/// * Pure memcpy out of an owned `Vec` — no allocation, no panic path,
///   no unwind across the `extern "C"` frame.
/// * Reports zero bytes only when the buffer is genuinely exhausted.
///   libghostty treats that as *permanent* EOF (`io.h:28-53`), which is
///   why [`SnapshotDecoder`] only calls into the decoder once the record
///   it will consume is fully buffered.
/// * Never calls back into the decoder that owns it
///   (`snapshot.h:342-354`).
unsafe extern "C" fn snapshot_reader_trampoline(
    userdata: *mut c_void,
    buffer: *mut u8,
    capacity: usize,
    out_read: *mut usize,
) -> bool {
    if userdata.is_null() {
        return false;
    }
    // SAFETY: `userdata` is the address of the `UnsafeCell<ReaderState>`
    // the decoder pinned in a `Box` for its whole lifetime. libghostty
    // calls this synchronously from inside `decoder_ready`/`decoder_next`
    // and the wrapper holds no other borrow of the state across those
    // calls.
    let state = unsafe { &mut *(userdata as *mut ReaderState) };
    if buffer.is_null() || out_read.is_null() {
        state.fault = true;
        return false;
    }
    let remaining = state.buf.len().saturating_sub(state.cursor);
    let take = remaining.min(capacity);
    if take > 0 {
        // SAFETY: `take` bytes are in range of both the owned buffer at
        // `cursor` and the caller's `capacity`-byte destination, and the
        // two allocations never overlap.
        unsafe {
            ptr::copy_nonoverlapping(state.buf.as_ptr().add(state.cursor), buffer, take);
        }
        state.cursor += take;
    }
    // SAFETY: non-null per the guard above; libghostty owns a live
    // `size_t` here for the duration of the call.
    unsafe { *out_read = take };
    true
}

/// Framing scanner state — record boundaries only.
struct Framing {
    /// Offset of the next record header to parse. Starts past the
    /// envelope, which is a fixed-size skip.
    at: usize,
    /// The READY record is fully buffered, so `decoder_ready` can run
    /// entirely from memory.
    ready_seen: bool,
    /// End offset of the last fully buffered PAGE or FINISH record —
    /// the records `decoder_next` stops on. Because scanning is a strict
    /// left-to-right prefix walk, `last_stop > consumed` is exactly
    /// "everything the next `decoder_next` will read is in memory".
    last_stop: usize,
    /// FINISH has been scanned; everything after it is trailing
    /// transport, not records (`snapshot.h:100-105`).
    finish_seen: bool,
}

/// Streaming snapshot decoder: feed bytes, take the terminal at READY,
/// prepend history pages as they arrive.
///
/// # Terminal access
///
/// The decoder hands out `&Terminal` and a pair of narrow forwarders
/// ([`Self::vt_write`], [`Self::resize`]) — never `&mut Terminal`. A
/// mutable borrow could be `mem::swap`ped out, which would free the very
/// handle `decoder_next` writes history into; the restriction makes that
/// unrepresentable rather than merely documented.
///
/// # Drop order
///
/// `Drop` frees the C decoder **first**, then the terminal, then the
/// buffer. The decoder borrows the terminal until FINISH validates or it
/// is freed (`snapshot.h:441-444`) and reads through a pointer into the
/// buffer, so any other order is a use-after-free. `finish`/`abandon`
/// take the fields out, so neither can double-free.
pub struct SnapshotDecoder {
    decoder: Option<sys::GhosttySnapshotDecoder>,
    terminal: Option<Terminal>,
    /// Boxed so its address is stable: the `GhosttyReader` the decoder
    /// copies at construction keeps this pointer for its whole lifetime.
    /// `UnsafeCell` because the trampoline mutates it from inside an FFI
    /// call that the wrapper entered through `&mut self`.
    reader: Box<UnsafeCell<ReaderState>>,
    opts: SnapshotDecodeOptions,
    framing: Framing,
    state: State,
    /// Source bytes the C decoder reports as consumed. Record-aligned,
    /// so it is the left edge of the next `decoder_next`'s reads.
    consumed: usize,
    extents: Option<(u64, Option<u64>)>,
    _not_sync: PhantomData<std::cell::Cell<()>>,
}

// SAFETY: the decoder handle, the decoded terminal, and the buffer can
// all move between threads as long as only one thread touches them at a
// time. `Sync` is deliberately not implemented (the `PhantomData` above),
// matching `Terminal`.
unsafe impl Send for SnapshotDecoder {}

impl SnapshotDecoder {
    pub fn new(opts: SnapshotDecodeOptions) -> Self {
        Self {
            decoder: None,
            terminal: None,
            reader: Box::new(UnsafeCell::new(ReaderState {
                buf: Vec::new(),
                cursor: 0,
                fault: false,
            })),
            opts,
            framing: Framing {
                at: ENVELOPE_LEN,
                ready_seen: false,
                last_stop: 0,
                finish_seen: false,
            },
            state: State::Feeding,
            consumed: 0,
            extents: None,
            _not_sync: PhantomData,
        }
    }

    /// Decode a complete snapshot in one call.
    ///
    /// Drives the same state machine the streaming path uses — the C
    /// one-shot `decoder_decode` is deliberately not wrapped, so there is
    /// only ever one code path to reason about. A truncated stream
    /// surfaces as [`Error::InvalidValue`], matching how libghostty
    /// reports a snapshot that ends before a required marker. Failure is
    /// transactional in the sense that matters: nothing partial escapes,
    /// the partially decoded terminal is abandoned and dropped.
    pub fn decode_bytes(bytes: Vec<u8>, opts: SnapshotDecodeOptions) -> Result<DecodedTerminal> {
        let mut decoder = Self::new(opts);
        decoder.feed_vec(bytes)?;
        if decoder.try_ready()? == ReadyState::NeedMoreBytes {
            return Err(Error::InvalidValue);
        }
        loop {
            match decoder.try_next()? {
                HistoryStep::Finished => break,
                HistoryStep::NeedMoreBytes => return Err(Error::InvalidValue),
                HistoryStep::Page { .. } => {}
            }
        }
        decoder.finish()
    }

    /// Append transport bytes and advance the framing scanner.
    ///
    /// Cap violations (total size, a single record's declared payload,
    /// and — when `max_continuation_bytes` is set — the CONTINUATION
    /// record's declared payload, which the format defines as exactly the
    /// continuation byte count) fail here, before any FFI call. Such a
    /// failure is sticky rather than poisoning: the offending record is
    /// still in the buffer, so every later call that rescans reports the
    /// same error. The consumer's answer is to drop the attach.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<()> {
        self.check_feedable()?;
        if bytes.is_empty() {
            return Ok(());
        }
        let max_total_bytes = self.opts.max_total_bytes;
        let reader = self.reader_mut();
        let total = reader
            .buf
            .len()
            .checked_add(bytes.len())
            .ok_or(Error::LimitExceeded)?;
        if total > max_total_bytes {
            return Err(Error::LimitExceeded);
        }
        reader
            .buf
            .try_reserve(bytes.len())
            .map_err(|_| Error::OutOfMemory)?;
        reader.buf.extend_from_slice(bytes);
        self.advance()
    }

    /// Feed an owned buffer, moving it in when nothing has been fed yet.
    /// Saves the buffered path a full copy of the snapshot.
    fn feed_vec(&mut self, bytes: Vec<u8>) -> Result<()> {
        self.check_feedable()?;
        if bytes.len() > self.opts.max_total_bytes {
            return Err(Error::LimitExceeded);
        }
        let reader = self.reader_mut();
        if reader.buf.is_empty() {
            reader.buf = bytes;
            return self.advance();
        }
        self.feed(&bytes)
    }

    fn check_feedable(&self) -> Result<()> {
        match self.state {
            State::Feeding | State::Ready => Ok(()),
            State::Finished => Err(Error::Lifecycle("feed after the snapshot finished")),
            State::Poisoned => Err(Error::Lifecycle("feed on a poisoned decoder")),
        }
    }

    /// Decode the renderable prefix through READY, if it is buffered.
    ///
    /// Calls `decoder_ready` at most once; a second call is
    /// [`Error::Lifecycle`], not a retry. On success the caller-owned
    /// terminal is adopted and the advisory history extents are cached
    /// immediately, because READY is their availability window.
    pub fn try_ready(&mut self) -> Result<ReadyState> {
        match self.state {
            State::Feeding => {}
            State::Ready | State::Finished => {
                return Err(Error::Lifecycle("try_ready called more than once"))
            }
            State::Poisoned => return Err(Error::Lifecycle("try_ready on a poisoned decoder")),
        }
        self.advance()?;
        if !self.framing.ready_seen {
            return Ok(ReadyState::NeedMoreBytes);
        }
        let decoder = self.ensure_decoder()?;
        let mut handle: sys::GhosttyTerminal = ptr::null_mut();
        // SAFETY: `decoder` is live and un-started; `handle` is a real
        // local of the out-parameter type.
        let rc = unsafe { sys::ghostty_snapshot_decoder_ready(decoder, &mut handle) };
        self.check_ffi(rc)?;
        if handle.is_null() {
            self.state = State::Poisoned;
            return Err(Error::NullHandle);
        }
        // Adopt the handle *before* reading any decoder data: a failing
        // getter poisons, and `abandon` must still be able to hand the
        // caller the terminal libghostty has already given us.
        // SAFETY: non-null, uniquely owned, straight out of a successful
        // `decoder_ready`; this decoder's `Drop`/`finish`/`abandon` free
        // the C decoder before the `Terminal`.
        self.terminal = Some(unsafe { Terminal::from_decoded(handle) });
        self.state = State::Ready;
        match self.read_ready_data(decoder) {
            Ok((primary, alternate, consumed)) => {
                self.extents = Some((primary, alternate));
                self.consumed = consumed;
                Ok(ReadyState::Ready)
            }
            Err(err) => {
                self.state = State::Poisoned;
                Err(err)
            }
        }
    }

    /// Consume one history page, if the next one is buffered.
    ///
    /// Progress data is read immediately after each success — a later
    /// `next` replaces or clears it (`snapshot.h:217-245`).
    pub fn try_next(&mut self) -> Result<HistoryStep> {
        match self.state {
            State::Ready => {}
            State::Finished => return Ok(HistoryStep::Finished),
            State::Feeding => {
                return Err(Error::Lifecycle("try_next before try_ready reported Ready"))
            }
            State::Poisoned => return Err(Error::Lifecycle("try_next on a poisoned decoder")),
        }
        self.advance()?;
        if self.framing.last_stop <= self.consumed {
            return Ok(HistoryStep::NeedMoreBytes);
        }
        let Some(decoder) = self.decoder else {
            return Err(Error::Lifecycle("try_next without a started decoder"));
        };
        // SAFETY: `decoder` is live and past READY.
        let rc = unsafe { sys::ghostty_snapshot_decoder_next(decoder) };
        if let Some(err) = self.take_reader_fault() {
            self.state = State::Poisoned;
            return Err(err);
        }
        match Error::from_result(rc) {
            Ok(()) => match self.read_progress(decoder) {
                Ok(step) => Ok(step),
                Err(err) => {
                    self.state = State::Poisoned;
                    Err(err)
                }
            },
            Err(Error::NoValue) => {
                self.state = State::Finished;
                match get_usize(
                    decoder,
                    sys::GhosttySnapshotDecoderData_GHOSTTY_SNAPSHOT_DECODER_DATA_SOURCE_OFFSET,
                ) {
                    Ok(offset) => {
                        self.consumed = offset;
                        Ok(HistoryStep::Finished)
                    }
                    Err(err) => {
                        self.state = State::Poisoned;
                        Err(err)
                    }
                }
            }
            Err(err) => {
                self.state = State::Poisoned;
                Err(err)
            }
        }
    }

    /// The decoded terminal, once READY has landed. Read-only on
    /// purpose — see the type's "Terminal access" note.
    pub fn terminal(&self) -> Option<&Terminal> {
        self.terminal.as_ref()
    }

    /// Advisory complete history extent for the primary screen, cached
    /// at READY. `None` before READY.
    pub fn history_rows_primary(&self) -> Option<u64> {
        self.extents.map(|(primary, _)| primary)
    }

    /// Advisory complete history extent for the alternate screen.
    /// `None` before READY *and* when the snapshot declares no alternate
    /// screen; the two are distinguished by
    /// [`Self::history_rows_primary`] being `Some`.
    pub fn history_rows_alternate(&self) -> Option<u64> {
        self.extents.and_then(|(_, alternate)| alternate)
    }

    /// Feed live PTY input to the decoded terminal between history
    /// pages, which `snapshot.h:462-477` explicitly allows.
    pub fn vt_write(&mut self, data: &[u8]) -> Result<()> {
        let terminal = self
            .terminal
            .as_mut()
            .ok_or(Error::Lifecycle("vt_write before try_ready reported Ready"))?;
        terminal.vt_write(data);
        Ok(())
    }

    /// Resize the decoded terminal between history pages.
    ///
    /// **A resize before FINISH forfeits the history that has not landed
    /// yet**: pages that can no longer be applied are still consumed and
    /// validated, but report zero rows. That is accepted architecture —
    /// re-attaching recovers the full scrollback — not a bug to work
    /// around here.
    pub fn resize(
        &mut self,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) -> Result<()> {
        let terminal = self
            .terminal
            .as_mut()
            .ok_or(Error::Lifecycle("resize before try_ready reported Ready"))?;
        terminal.resize(cols, rows, cell_width_px, cell_height_px)
    }

    /// Take the decoded terminal once FINISH has validated.
    ///
    /// Legal only in the finished state; anything earlier is
    /// [`Error::Lifecycle`] and, because the signature consumes `self`,
    /// drops the decoder — calling `finish` early is a caller bug, not a
    /// recoverable path. Frees the C decoder before moving the terminal
    /// out, per the type's drop-order note.
    pub fn finish(mut self) -> Result<DecodedTerminal> {
        if self.state != State::Finished {
            return Err(Error::Lifecycle("finish before the decoder reached FINISH"));
        }
        // `SOURCE_OFFSET` lives on the decoder that `free_decoder` below
        // destroys; `try_next` cached it when FINISH validated.
        let source_offset = self.consumed;
        let (history_rows_primary, history_rows_alternate) = self.extents.unwrap_or((0, None));
        self.free_decoder();
        let terminal = self
            .terminal
            .take()
            .ok_or(Error::Lifecycle("finish without a decoded terminal"))?;
        Ok(DecodedTerminal {
            terminal,
            source_offset,
            history_rows_primary,
            history_rows_alternate,
        })
    }

    /// Give up on the rest of the history and keep whatever landed.
    ///
    /// Legal from every state, including poisoned — the header sanctions
    /// it ("abandoning an incremental decode leaves that terminal
    /// usable", `snapshot.h:391-402`). `None` before READY, when there is
    /// no terminal yet.
    pub fn abandon(mut self) -> Option<Terminal> {
        self.free_decoder();
        self.terminal.take()
    }

    fn reader_mut(&mut self) -> &mut ReaderState {
        // SAFETY: the wrapper only takes this borrow between FFI calls;
        // libghostty's reader callback runs synchronously inside
        // `decoder_ready`/`decoder_next` and no borrow is held across
        // those.
        unsafe { &mut *self.reader.get() }
    }

    /// Walk record headers from wherever the last scan stopped,
    /// recording boundaries and enforcing the wrapper's per-record caps.
    /// Boundary-finding only: unknown tags are skipped by their declared
    /// length and left for libghostty to reject.
    fn advance(&mut self) -> Result<()> {
        // SAFETY: same borrow discipline as `reader_mut`; this one is
        // read-only and ends before the function returns. It is legal to
        // mutate `self.framing` while `buf` is live only because the
        // `ReaderState` lives in its own heap allocation — but no FFI
        // (nothing reader-reentrant) may be called while `buf` is held.
        let buf = unsafe { &(*self.reader.get()).buf };
        while !self.framing.finish_seen {
            let start = self.framing.at;
            let Some(header_end) = start.checked_add(RECORD_HEADER_LEN) else {
                return Err(Error::InvalidValue);
            };
            if buf.len() < header_end {
                break;
            }
            let tag = u16::from_le_bytes([buf[start], buf[start + 1]]);
            let payload_len = u32::from_le_bytes([
                buf[start + 2],
                buf[start + 3],
                buf[start + 4],
                buf[start + 5],
            ]) as usize;
            if payload_len > self.opts.max_record_bytes {
                return Err(Error::LimitExceeded);
            }
            if tag == TAG_CONTINUATION {
                if let Some(max) = self.opts.max_continuation_bytes {
                    if payload_len > max {
                        return Err(Error::LimitExceeded);
                    }
                }
            }
            let Some(record_end) = header_end.checked_add(payload_len) else {
                return Err(Error::InvalidValue);
            };
            if buf.len() < record_end {
                break;
            }
            match tag {
                TAG_READY => self.framing.ready_seen = true,
                TAG_PAGE => self.framing.last_stop = record_end,
                TAG_FINISH => {
                    self.framing.last_stop = record_end;
                    self.framing.finish_seen = true;
                }
                _ => {}
            }
            self.framing.at = record_end;
        }
        Ok(())
    }

    /// Build the C decoder over the cursor-reader and apply the options.
    ///
    /// Deferred to the first `try_ready` because options may only be set
    /// before decoding starts, and built over a reader rather than
    /// `decoder_new_buf` because the buffer grows between calls — a
    /// borrowed-buffer decoder would be left pointing at a stale
    /// allocation the first time `feed` reallocates.
    fn ensure_decoder(&mut self) -> Result<sys::GhosttySnapshotDecoder> {
        if let Some(decoder) = self.decoder {
            return Ok(decoder);
        }
        let reader = sys::GhosttyReader {
            read: Some(snapshot_reader_trampoline),
            userdata: self.reader.get().cast(),
        };
        let mut decoder: sys::GhosttySnapshotDecoder = ptr::null_mut();
        // SAFETY: null allocator selects libghostty's default; the out
        // pointer is a real local; `reader`'s userdata is the boxed
        // `ReaderState`, which outlives the decoder (freed first in
        // `Drop`).
        let rc = unsafe { sys::ghostty_snapshot_decoder_new(ptr::null(), &mut decoder, reader) };
        Error::from_result(rc)?;
        if decoder.is_null() {
            return Err(Error::NullHandle);
        }
        if let Err(err) = apply_options(decoder, &self.opts) {
            // SAFETY: freshly created, never started, freed exactly once.
            unsafe { sys::ghostty_snapshot_decoder_free(decoder) };
            return Err(err);
        }
        self.decoder = Some(decoder);
        Ok(decoder)
    }

    /// Map an FFI result from `decoder_ready`, preferring a wrapper-side
    /// cause over the `GHOSTTY_IO_ERROR` it would surface as. Any failure
    /// after input consumption poisons the decoder (`snapshot.h:447-450`);
    /// the wrapper poisons on every FFI failure, which is the
    /// conservative superset.
    fn check_ffi(&mut self, rc: i32) -> Result<()> {
        if let Some(err) = self.take_reader_fault() {
            self.state = State::Poisoned;
            return Err(err);
        }
        if let Err(err) = Error::from_result(rc) {
            self.state = State::Poisoned;
            return Err(err);
        }
        Ok(())
    }

    fn take_reader_fault(&mut self) -> Option<Error> {
        let reader = self.reader_mut();
        if reader.fault {
            reader.fault = false;
            return Some(Error::NullHandle);
        }
        None
    }

    fn read_ready_data(
        &self,
        decoder: sys::GhosttySnapshotDecoder,
    ) -> Result<(u64, Option<u64>, usize)> {
        let primary = get_u64(
            decoder,
            sys::GhosttySnapshotDecoderData_GHOSTTY_SNAPSHOT_DECODER_DATA_HISTORY_ROWS_PRIMARY,
        )?;
        let alternate = match get_u64(
            decoder,
            sys::GhosttySnapshotDecoderData_GHOSTTY_SNAPSHOT_DECODER_DATA_HISTORY_ROWS_ALTERNATE,
        ) {
            Ok(rows) => Some(rows),
            Err(Error::NoValue) => None,
            Err(err) => return Err(err),
        };
        let consumed = get_usize(
            decoder,
            sys::GhosttySnapshotDecoderData_GHOSTTY_SNAPSHOT_DECODER_DATA_SOURCE_OFFSET,
        )?;
        Ok((primary, alternate, consumed))
    }

    fn read_progress(&mut self, decoder: sys::GhosttySnapshotDecoder) -> Result<HistoryStep> {
        let mut screen: sys::GhosttyTerminalScreen =
            sys::GhosttyTerminalScreen_GHOSTTY_TERMINAL_SCREEN_PRIMARY;
        // SAFETY: decoder live and just past a successful `next`;
        // `screen` is a real local of the documented out type.
        let rc = unsafe {
            sys::ghostty_snapshot_decoder_get(
                decoder,
                sys::GhosttySnapshotDecoderData_GHOSTTY_SNAPSHOT_DECODER_DATA_PROGRESS_SCREEN,
                (&mut screen) as *mut sys::GhosttyTerminalScreen as *mut _,
            )
        };
        Error::from_result(rc)?;
        let rows_prepended = get_usize(
            decoder,
            sys::GhosttySnapshotDecoderData_GHOSTTY_SNAPSHOT_DECODER_DATA_PROGRESS_ROWS,
        )?;
        let pages_remaining_in_screen = get_u32(
            decoder,
            sys::GhosttySnapshotDecoderData_GHOSTTY_SNAPSHOT_DECODER_DATA_PROGRESS_REMAINING,
        )?;
        self.consumed = get_usize(
            decoder,
            sys::GhosttySnapshotDecoderData_GHOSTTY_SNAPSHOT_DECODER_DATA_SOURCE_OFFSET,
        )?;
        Ok(HistoryStep::Page {
            screen: ActiveScreen::from_sys(screen),
            rows_prepended,
            pages_remaining_in_screen,
        })
    }

    fn free_decoder(&mut self) {
        if let Some(decoder) = self.decoder.take() {
            // SAFETY: created by `decoder_new`, taken out of the field so
            // it can be freed exactly once.
            unsafe { sys::ghostty_snapshot_decoder_free(decoder) };
        }
    }
}

impl Drop for SnapshotDecoder {
    fn drop(&mut self) {
        // Order is load-bearing: the C decoder borrows both the terminal
        // and the reader's buffer, so it dies first. The terminal follows
        // (its own `Drop` calls `ghostty_terminal_free`), then the boxed
        // buffer with the remaining fields.
        self.free_decoder();
        drop(self.terminal.take());
    }
}

fn apply_options(decoder: sys::GhosttySnapshotDecoder, opts: &SnapshotDecodeOptions) -> Result<()> {
    if let Some(max) = opts.max_continuation_bytes {
        // SAFETY: decoder live and un-started; `max` is a live local of
        // the `size_t` type this option documents.
        let rc = unsafe {
            sys::ghostty_snapshot_decoder_set(
                decoder,
                sys::GhosttySnapshotDecoderOption_GHOSTTY_SNAPSHOT_DECODER_OPT_MAX_CONTINUATION_BYTES,
                (&raw const max).cast(),
            )
        };
        Error::from_result(rc)?;
    }
    let retain = opts.retain_continuation;
    // SAFETY: same, with the `bool` this option documents.
    let rc = unsafe {
        sys::ghostty_snapshot_decoder_set(
            decoder,
            sys::GhosttySnapshotDecoderOption_GHOSTTY_SNAPSHOT_DECODER_OPT_RETAIN_CONTINUATION,
            (&raw const retain).cast(),
        )
    };
    Error::from_result(rc)
}

fn get_usize(
    decoder: sys::GhosttySnapshotDecoder,
    key: sys::GhosttySnapshotDecoderData,
) -> Result<usize> {
    let mut out: usize = 0;
    // SAFETY: decoder live; `out` is a real local of the `size_t` type
    // these data keys document.
    let rc = unsafe {
        sys::ghostty_snapshot_decoder_get(decoder, key, (&mut out) as *mut usize as *mut _)
    };
    Error::from_result(rc)?;
    Ok(out)
}

fn get_u64(
    decoder: sys::GhosttySnapshotDecoder,
    key: sys::GhosttySnapshotDecoderData,
) -> Result<u64> {
    let mut out: u64 = 0;
    // SAFETY: decoder live; `out` is a real local of the `uint64_t` type
    // these data keys document.
    let rc = unsafe {
        sys::ghostty_snapshot_decoder_get(decoder, key, (&mut out) as *mut u64 as *mut _)
    };
    Error::from_result(rc)?;
    Ok(out)
}

fn get_u32(
    decoder: sys::GhosttySnapshotDecoder,
    key: sys::GhosttySnapshotDecoderData,
) -> Result<u32> {
    let mut out: u32 = 0;
    // SAFETY: decoder live; `out` is a real local of the `uint32_t` type
    // this data key documents.
    let rc = unsafe {
        sys::ghostty_snapshot_decoder_get(decoder, key, (&mut out) as *mut u32 as *mut _)
    };
    Error::from_result(rc)?;
    Ok(out)
}
