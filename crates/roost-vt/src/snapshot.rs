//! Safe wrapper around `ghostty_snapshot_*` — the terminal snapshot
//! format that host-sessions (HS-1) uses as its attach payload.
//!
//! Only the encode half exists so far: [`Terminal::snapshot`] serializes
//! a terminal to a byte vector through `ghostty_snapshot_encode`.
//!
//! # No Swift twin
//!
//! Every other libghostty capability roost uses is mirrored on the Mac
//! side, but the Swift app calls `ghostty_terminal_new` and friends
//! directly and never consumes `roost-vt`. Host sessions ship with the
//! iced UI as their only client (roadmap D1/D5), so there is
//! deliberately no Swift counterpart to this API and `mac/` stays
//! untouched. Revisit only if the Mac UI becomes a host-sessions client.

use std::os::raw::c_void;

use crate::sys;
use crate::terminal::Terminal;
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
