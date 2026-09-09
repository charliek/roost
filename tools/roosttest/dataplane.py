"""Speak a host session's attach data plane from Python.

A data connection is the one place in the protocol where the wire stops
being newline JSON. It opens with a handshake line, gets one JSON line
back, and — if that line said `ok` — turns binary for the rest of its
life: the 8-byte `ROOSTDP2` preamble, then length-prefixed frames in
both directions (`crates/roost-ipc/src/dataframe.rs`,
`docs/reference/ipc.md`'s data-plane section).

```text
frame := u32-LE payload length | u8 type | payload
server → client:  SNAP 0x01 | PTY 0x02 | EXIT 0x03 | ERROR 0x0F
client → server:  INPUT 0x11 | RESIZE 0x12
```

`client.Roost` cannot do this — its whole shape is one request, one
response — so this is its own module for the same reason
`eventstream.py` is.

# What this deliberately does NOT do

It never decodes a payload. Neither kind's bytes are interpreted here:

* **`ghostty-snapshot`** rides libghostty's own binary format, and there
  is exactly one client implementation of it in this repo — the Rust
  integration client (`crates/roost-session/tests/attach_stream_test.rs`),
  which drives the real `SnapshotDecoder`. A second, Python-side decoder
  would be a re-implementation that could agree with the wire while both
  disagreed with libghostty, which is the failure the
  single-implementation rule exists to prevent (architecture §12).
* **`vt`** is a VT byte stream, and replaying one is a terminal
  emulator's job. The fidelity proof for `vt` is likewise Rust-side
  (`roost-vt`'s corpus and `attach_stream_test`), where libghostty is
  the parser.

What this module does instead is **frame accounting** — plus, for
GHOSTSNP, a walk of its record *boundaries*: the envelope is 10 bytes
and each record header is `u16 tag | u32 len | u32 crc`, so finding
boundaries needs no knowledge of any payload. That is enough to answer
the questions the contract lane asks: did READY arrive, where, did it
precede the history behind it, did FINISH land under load. The tags are
boundary markers here, never semantics.

# What it does do

* [`SnapScanner`] — reassembles a GHOSTSNP `SNAP` byte stream across
  frames (frames carry arbitrary byte windows; records are NOT
  frame-aligned) and walks record boundaries.
* [`VtPayload`] — the `vt` counterpart: it concatenates the payload and
  watches for the one zero-length `SNAP` frame that closes it. Its
  [`VtPayload.plain_rows`] is a row *count and position* cross-check
  against `tab.dump`, not a replay — see the class docstring.
* [`DataPlane`] — the connection: handshake, preamble, framed reads with
  cross-read reassembly, `INPUT`/`RESIZE` writes.
* The invariants that hold on every connection, checked as frames
  arrive rather than restated in each test — so a violation fails at the
  frame that broke it, with the frame in hand:
  - **seq contiguity**: the fence `S` from the handshake reply means the
    first `PTY` frame is `S+1` and every one after it is exactly one
    more. A gap is the one thing the protocol promises can never happen;
  - **terminal frames**: `ERROR` and `EXIT` both end the connection, so
    anything behind either is a protocol violation;
  - **nothing precedes readiness**: in snapshot mode the prefix goes out
    whole, so no `PTY` or `EXIT` frame may arrive before the payload's
    readiness boundary lands — GHOSTSNP's READY record, or, under `vt`,
    the empty-SNAP terminator that ends the whole payload. Resume
    streams are exempt — they carry no payload;
  - **the end marker closes the payload**: no `SNAP` frame follows
    GHOSTSNP's FINISH record or `vt`'s empty terminator frame.

Every public entry point takes a raw budget, scales it once through
`client.scaled_timeout`, and turns it into an absolute deadline that
every `recv` beneath it is charged against — so a frame split across
several reads is bounded by the caller's clock, not by a fresh budget
per read.
"""

from __future__ import annotations

import json
import socket
import struct
import time
from dataclasses import dataclass, field

from client import RoostError, scaled_timeout

# ---------------------------------------------------------------------------
# Wire constants — mirrors of `roost_ipc::dataframe` and
# `roost_ipc::messages`. Restated rather than generated, on purpose: a
# change to either fails here instead of being absorbed.
# ---------------------------------------------------------------------------

PREAMBLE = b"ROOSTDP2"
FRAME_HEADER_LEN = 5
MAX_DATA_FRAME_BYTES = 1024 * 1024

FRAME_SNAP = 0x01
FRAME_PTY = 0x02
FRAME_EXIT = 0x03
FRAME_ERROR = 0x0F
FRAME_INPUT = 0x11
FRAME_RESIZE = 0x12

FRAME_NAMES = {
    FRAME_SNAP: "SNAP",
    FRAME_PTY: "PTY",
    FRAME_EXIT: "EXIT",
    FRAME_ERROR: "ERROR",
    FRAME_INPUT: "INPUT",
    FRAME_RESIZE: "RESIZE",
}

SESSION_PROTOCOL_VERSION = 4
GHOSTTY_SNAPSHOT = "ghostty-snapshot"
VT = "vt"
#: `roost_engine::attach::VT_SNAP_FRAME_BYTES`. Not the wire's 1 MiB
#: cap: a `vt` attach holds every PTY frame behind its whole payload and
#: drains the tee only between writes, so the frame size is what bounds
#: how long a busy tab goes unread.
VT_SNAP_FRAME_BYTES = 64 * 1024

# ---------------------------------------------------------------------------
# GHOSTSNP framing — TAG SCANNING ONLY (see the module docstring)
# ---------------------------------------------------------------------------

SNAPSHOT_MAGIC = b"GHOSTSNP"
#: `"GHOSTSNP"` + a u16-LE format version.
ENVELOPE_LEN = 10
#: `u16 tag | u32 payload_len | u32 crc`.
RECORD_HEADER_LEN = 10

# The format's own tag table (`ghostty/src/terminal/snapshot/record.zig`'s
# `Tag`). `roost_vt::snapshot` names only the four its decoder stops on;
# the ordering assertions here need the whole set, because what READY
# separates is "the live screen" from "the history behind it".
TAG_TERMINAL = 1
TAG_SCREEN = 2
TAG_PAGE = 3
TAG_HISTORY = 4
TAG_READY = 5
TAG_FINISH = 6
TAG_CONTINUATION = 7


# ---------------------------------------------------------------------------
# Frames
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Frame:
    """One decoded frame. The type stays a raw byte for the same reason
    the Rust framer keeps it one: an unknown type is a protocol error the
    endpoint reports *with the byte it saw*, not a decode failure that
    loses it."""

    frame_type: int
    payload: bytes

    @property
    def name(self) -> str:
        return FRAME_NAMES.get(self.frame_type, f"0x{self.frame_type:02x}")

    def pty(self) -> tuple[int, bytes]:
        """`(seq, bytes)` — the u64-LE sequence number and the raw PTY
        payload behind it."""
        assert self.frame_type == FRAME_PTY, f"not a PTY frame: {self.name}"
        assert len(self.payload) >= 9, f"a PTY frame carries seq + bytes: {len(self.payload)}"
        return struct.unpack_from("<Q", self.payload)[0], self.payload[8:]

    def exit(self) -> tuple[int, int]:
        """`(final_seq, exit_code)`."""
        assert self.frame_type == FRAME_EXIT, f"not an EXIT frame: {self.name}"
        assert len(self.payload) == 12, f"an EXIT frame is u64 + i32: {len(self.payload)}"
        return struct.unpack("<Qi", self.payload)

    def error(self) -> dict:
        """The `{code, message}` diagnostic. The connection closes after
        one of these."""
        assert self.frame_type == FRAME_ERROR, f"not an ERROR frame: {self.name}"
        return json.loads(self.payload.decode())


# ---------------------------------------------------------------------------
# The SNAP byte stream
# ---------------------------------------------------------------------------


class SnapScanner:
    """Reassemble the `SNAP` byte stream and walk its record boundaries.

    Frames carry arbitrary windows of the encoded snapshot — the server
    splits on the 1 MiB frame cap and on the READY boundary, never on
    records — so a record routinely straddles two frames and the walk has
    to buffer across them.

    Payloads are skipped, never read: this knows a record's *length*,
    which is all a boundary walk needs. Consumed bytes are released as
    the cursor passes them, so a 2000-line snapshot does not sit in
    memory twice.
    """

    #: What the two boundaries are called on the wire. [`DataPlane`]
    #: reads these so its waits and its violation messages name the
    #: marker of whichever kind is being served.
    ready_name = "the snapshot's READY record"
    end_name = "the snapshot's FINISH record"

    def __init__(self) -> None:
        self._buf = bytearray()
        #: Absolute offset (into the SNAP byte stream) of `self._buf[0]`.
        self._base = 0
        self._envelope_read = False
        self.total_bytes = 0
        #: Record tags in arrival order — the ordering assertions read
        #: this (READY before the history manifest, FINISH last).
        self.tags: list[int] = []
        self.ready_seen = False
        self.finish_seen = False
        #: Byte offset, in the SNAP stream, of the first byte AFTER the
        #: READY record — the same boundary `roost_vt::ready_boundary`
        #: computes server-side, which is where the server stops sending
        #: at full speed and starts interleaving.
        self.ready_offset: int | None = None
        self.format_version: int | None = None

    def feed(self, payload: bytes) -> None:
        self.total_bytes += len(payload)
        self._buf += payload
        self._walk()

    def _walk(self) -> None:
        at = 0
        if not self._envelope_read:
            if len(self._buf) < ENVELOPE_LEN:
                return
            magic = bytes(self._buf[: len(SNAPSHOT_MAGIC)])
            assert magic == SNAPSHOT_MAGIC, (
                f"the SNAP stream does not open with GHOSTSNP: {magic!r}"
            )
            self.format_version = struct.unpack_from("<H", self._buf, len(SNAPSHOT_MAGIC))[0]
            self._envelope_read = True
            at = ENVELOPE_LEN
        while True:
            if len(self._buf) - at < RECORD_HEADER_LEN:
                break
            tag, payload_len = struct.unpack_from("<HI", self._buf, at)
            record_end = at + RECORD_HEADER_LEN + payload_len
            if len(self._buf) < record_end:
                break
            at = record_end
            self.tags.append(tag)
            if tag == TAG_READY and not self.ready_seen:
                self.ready_seen = True
                self.ready_offset = self._base + at
            elif tag == TAG_FINISH:
                self.finish_seen = True
        # Release what the walk is past; `_base` keeps offsets absolute.
        if at:
            del self._buf[:at]
            self._base += at

    def assert_ready_leads(self, *, expect_history: bool) -> None:
        """READY separates the live screen from the history behind it.

        The whole point of the boundary is that a client can render as
        soon as it has READY: the terminal and its live screen are in
        front of it, and the `history` manifest — the catch-up a client
        cannot place until it has somewhere to put it — comes after.
        FINISH closes the stream and is always last.

        `expect_history` is required rather than defaulted because
        "there was no history record" and "history was correctly behind
        READY" are the same observation on a tab with no scrollback, and
        a caller that seeded scrollback must not silently accept the
        first. Pass `True` wherever the tab was seeded past its viewport.
        """
        assert self.ready_seen, f"no READY record in the snapshot (tags={self.tags})"
        ready_at = self.tags.index(TAG_READY)
        history = [i for i, tag in enumerate(self.tags) if tag == TAG_HISTORY]
        assert all(i > ready_at for i in history), (
            f"history arrived before READY (tags={self.tags})"
        )
        if expect_history:
            assert history, (
                f"this tab was seeded past its viewport but the snapshot carried no "
                f"history manifest (tags={self.tags})"
            )
        # The live-state records are the READY prefix by definition: one
        # arriving behind READY would mean a client that rendered at the
        # boundary rendered from an incomplete terminal.
        live = [i for i, tag in enumerate(self.tags) if tag in (TAG_TERMINAL, TAG_SCREEN)]
        assert live, f"the snapshot carried no terminal/screen records (tags={self.tags})"
        assert all(i < ready_at for i in live), (
            f"a terminal/screen record landed behind READY (tags={self.tags})"
        )
        if TAG_FINISH in self.tags:
            assert self.tags.index(TAG_FINISH) > ready_at, f"FINISH before READY: {self.tags}"
            assert self.tags[-1] == TAG_FINISH, f"FINISH is not the last record: {self.tags}"


# ---------------------------------------------------------------------------
# The `vt` payload
# ---------------------------------------------------------------------------


def strip_escapes(data: bytes) -> bytes:
    """Remove every escape sequence, leaving the printed bytes.

    Enough of the grammar to walk *past* a sequence, which is all the
    row accounting below needs: CSI (`ESC [` … final `0x40`-`0x7e`), OSC
    (`ESC ]` … `BEL` or `ESC \\`), the other string sequences (DCS, SOS,
    PM, APC — same two terminators), and the two-or-three-byte escapes
    (`ESC ( B`, `ESC H`, …). An unterminated string sequence runs to the
    end of the payload, which is exactly what the retained continuation
    at the tail of a `vt` payload is.

    Nothing here interprets what it drops — a CSI is skipped, never
    applied. See [`VtPayload`] for why that is the whole point.
    """
    out = bytearray()
    at = 0
    end = len(data)
    while at < end:
        byte = data[at]
        if byte != 0x1B:
            out.append(byte)
            at += 1
            continue
        at += 1
        if at >= end:
            break
        opener = data[at]
        at += 1
        if opener == 0x5B:  # CSI: parameters/intermediates, then a final byte
            while at < end and 0x20 <= data[at] <= 0x3F:
                at += 1
            if at < end:
                at += 1
            continue
        if opener in (0x5D, 0x50, 0x58, 0x5E, 0x5F):  # OSC, DCS, SOS, PM, APC
            while at < end:
                if data[at] == 0x07:
                    at += 1
                    break
                if data[at] == 0x1B and at + 1 < end and data[at + 1] == 0x5C:
                    at += 2
                    break
                at += 1
            continue
        # Everything else: zero or more intermediates, then a final byte
        # that `opener` already is unless it was an intermediate.
        while 0x20 <= opener <= 0x2F and at < end:
            opener = data[at]
            at += 1
    return bytes(out)


class VtPayload:
    """Collect a `vt` payload and account for the frames carrying it.

    The `vt` kind has no framing of its own — it is a plain VT byte
    stream — so the only structure on the wire is the `SNAP` framing
    around it plus the **one zero-length `SNAP` frame** that closes it
    (`crates/roost-engine/src/attach.rs`). That marker is both the
    readiness boundary and the end of the payload, which is why
    `ready_seen` and `finish_seen` move together here where GHOSTSNP has
    two distinct records.

    There is no magic to check: the first byte of a `vt` payload is
    whatever the composition starts with.
    """

    ready_name = "the vt payload's empty-SNAP terminator"
    end_name = ready_name

    def __init__(self) -> None:
        self.payload = bytearray()
        self.total_bytes = 0
        self.ready_seen = False
        self.finish_seen = False
        #: Zero-length `SNAP` frames seen. The terminator is emitted
        #: unconditionally and exactly once; [`DataPlane`] rejects any
        #: `SNAP` frame behind it, so this can only ever be 0 or 1 — it
        #: is counted so a test can say "exactly one" directly.
        self.terminator_frames = 0

    def feed(self, payload: bytes) -> None:
        if not payload:
            self.terminator_frames += 1
            self.ready_seen = True
            self.finish_seen = True
            return
        assert len(payload) <= VT_SNAP_FRAME_BYTES, (
            f"a vt SNAP frame carried {len(payload)} bytes, past the "
            f"{VT_SNAP_FRAME_BYTES}-byte cap"
        )
        self.total_bytes += len(payload)
        self.payload += payload

    def plain_rows(self) -> list[str]:
        """The payload's rows, escapes removed, split on `\\r\\n` pairs.

        A cross-check, not a replay. The composition writes one `\\r\\n`
        per row and pads with bare ones up to the terminal's total row
        count (plan 053 §3.1 step 3), and cells hold no C0 bytes — so
        counting the pairs answers "did the payload carry a row for
        every row the server has" and "did the seeded markers land at
        the same offsets `tab.dump` reports them at". Nothing here
        knows what a CSI *does*: the escapes are skipped, and a row's
        text is whatever printable bytes were between two newlines.

        Split on the pair alone, never a lone `\\r` or `\\n`, because
        those two carry no row of their own in this stream.

        The tail row is the one to read with care: it holds the cursor
        cell the state pass re-prints, so it is a row of the terminal
        but not a full picture of it.
        """
        return [
            row.decode("utf-8", "replace")
            for row in strip_escapes(bytes(self.payload)).split(b"\r\n")
        ]


def collector_for(kind: str):
    """The payload accountant for a negotiated kind."""
    if kind == VT:
        return VtPayload()
    if kind == GHOSTTY_SNAPSHOT:
        return SnapScanner()
    raise AssertionError(f"no payload accountant for kind {kind!r}")


# ---------------------------------------------------------------------------
# The connection
# ---------------------------------------------------------------------------


def _optional_int(raw: dict, key: str) -> int | None:
    """A key the server sends only when it has something to say.

    Absent is a legal answer, so a miss is `None` rather than a
    `KeyError` — a client that treated the omission as malformed could
    not talk to a session that had nothing to report.
    """
    value = raw.get(key)
    return None if value is None else int(value)


@dataclass
class Reply:
    """The one JSON line a data connection gets before the wire turns
    binary — or the only line it gets, if the handshake was refused."""

    ok: bool
    raw: dict
    kind: str = ""
    mode: str = ""
    seq: int = 0
    server_epoch: int = 0
    tab_generation: int = 0
    #: The geometry the payload was encoded at, when the server said —
    #: only an unfocused snapshot attach, which by definition did not
    #: resize the tab to the client's own grid. `None` on a focused
    #: attach and on a resume, so an absent pair is an answer rather
    #: than a missing field.
    snapshot_cols: int | None = None
    snapshot_rows: int | None = None
    code: str = ""
    message: str = ""


@dataclass
class Ending:
    """How a data connection ended. `kind` is `"error"`, `"exit"` or
    `"eof"`; the others carry whichever detail that kind has."""

    kind: str
    code: str = ""
    message: str = ""
    final_seq: int = 0
    exit_code: int = 0
    frames: list[Frame] = field(default_factory=list)


class DataPlane:
    """One attach data connection.

    Open it, [`handshake`] with a ticket `tab.attach` handed out, then
    read frames. Contiguity, the payload accounting, and the
    terminal-frame bookkeeping all happen as frames arrive, so a test
    asserts on the *conclusion* rather than re-deriving it.

    `kind` is the payload kind this connection expects to be served.
    The handshake reply is what actually selects the accountant — it is
    the authoritative statement of what the server negotiated — and a
    reply naming a different kind than the caller asked for fails here
    rather than being scanned as the wrong format.
    """

    def __init__(self, socket_path, timeout: float = 15.0, kind: str = GHOSTTY_SNAPSHOT):
        self.path = str(socket_path)
        self.kind = kind
        self._buf = b""
        self._eof = False
        self._sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._sock.settimeout(scaled_timeout(timeout))
        self._sock.connect(self.path)

        self.reply: Reply | None = None
        #: Everything the server wrote after refusing a handshake. Must
        #: be empty: a rejection is a JSON line and a close, never a
        #: preamble the client would then try to parse.
        self.trailing = b""
        self.snap = collector_for(kind)
        self.frames_read = 0
        self.pty_frames = 0
        self.pty_bytes = 0
        #: `frames_read` at the moment the readiness boundary landed,
        #: and at the first PTY frame — the ordering assertions compare
        #: them.
        self.ready_at_frame: int | None = None
        self.first_pty_at_frame: int | None = None
        self.snap_frames = 0
        #: How many SNAP frames it took to deliver the ready prefix. The
        #: server cuts its first frame exactly on the READY boundary, so
        #: under GHOSTSNP this is 1 for any prefix under the 1 MiB frame
        #: cap. Under `vt` the whole payload is the prefix, so it counts
        #: every payload frame plus the terminator.
        self.snap_frames_at_ready: int | None = None
        #: The next seq a PTY frame must carry. Set from the handshake's
        #: fence and advanced on every frame.
        self.next_seq: int | None = None
        self.last_seq: int | None = None
        self.exit: tuple[int, int] | None = None
        self.error: dict | None = None
        #: `"EXIT"` or `"ERROR"` once one has arrived. Both are terminal
        #: by contract, so anything after one is a protocol violation
        #: rather than something a client should keep parsing.
        self.terminal_frame: str | None = None
        #: Whether this connection was served a snapshot. Set from the
        #: handshake reply, because the readiness rules apply to exactly
        #: one of the two modes: a resume stream carries no payload and
        #: therefore no boundary to order anything against.
        self.snapshot_mode: bool | None = None

    # -- lifecycle --------------------------------------------------------
    def close(self) -> None:
        try:
            self._sock.close()
        except OSError:
            pass

    def __enter__(self) -> "DataPlane":
        return self

    def __exit__(self, *_exc) -> None:
        self.close()

    # -- handshake --------------------------------------------------------
    def handshake(
        self,
        token: str,
        *,
        protocol_version: int = SESSION_PROTOCOL_VERSION,
        resume_from_seq: int | None = None,
        server_epoch: int | None = None,
        tab_generation: int | None = None,
        timeout: float = 30.0,
    ) -> Reply:
        """Send the handshake line and read the answer.

        Returns the [`Reply`] rather than raising on refusal: which code
        came back *is* the assertion in most of the rejection cases, and
        a refusal is a normal protocol outcome the way an `ok: false`
        response envelope is.

        On acceptance the 8-byte preamble is read and verified here, so
        the caller's next read is the first frame. On refusal the socket
        is read to EOF into [`trailing`] — the contract is one line and a
        close, and a rejected client must never be handed binary.

        The reply's `kind` is what the payload is read as: the data
        connection's own handshake is the authoritative statement of
        what was negotiated (plan 053 §3.3), so a client that guessed
        would be reading a format nobody promised it.
        """
        request: dict = {"attach": token, "protocol_version": protocol_version}
        if resume_from_seq is not None:
            request["resume_from_seq"] = resume_from_seq
        if server_epoch is not None:
            request["server_epoch"] = server_epoch
        if tab_generation is not None:
            request["tab_generation"] = tab_generation
        self._sock.sendall((json.dumps(request) + "\n").encode())

        deadline = time.monotonic() + scaled_timeout(timeout)
        raw = json.loads(self._readline(deadline))
        if raw.get("ok"):
            reply = Reply(
                ok=True,
                raw=raw,
                kind=raw["kind"],
                mode=raw["mode"],
                seq=int(raw["seq"]),
                server_epoch=int(raw["server_epoch"]),
                tab_generation=int(raw["tab_generation"]),
                snapshot_cols=_optional_int(raw, "snapshot_cols"),
                snapshot_rows=_optional_int(raw, "snapshot_rows"),
            )
            assert reply.kind == self.kind, (
                f"this connection was opened to read {self.kind!r} but the session "
                f"served {reply.kind!r}"
            )
            self.snap = collector_for(reply.kind)
            self._read_preamble(deadline)
            self.next_seq = reply.seq + 1
            self.snapshot_mode = reply.mode == "snapshot"
        else:
            err = raw.get("error") or {}
            reply = Reply(
                ok=False,
                raw=raw,
                code=err.get("code", "unknown"),
                message=err.get("message", ""),
            )
            self.trailing = self._read_to_eof(time.monotonic() + scaled_timeout(5.0))
        self.reply = reply
        return reply

    def _read_preamble(self, deadline: float) -> None:
        while len(self._buf) < len(PREAMBLE):
            if not self._fill(deadline):
                raise RoostError("disconnected", "the session closed before the preamble")
        got, self._buf = self._buf[: len(PREAMBLE)], self._buf[len(PREAMBLE) :]
        if got != PREAMBLE:
            raise AssertionError(f"bad data-plane preamble: {got!r} (want {PREAMBLE!r})")

    # -- reading ----------------------------------------------------------
    #
    # Every public entry point takes a RAW budget in seconds, scales it
    # once by `ROOST_TEST_TIMEOUT_SCALE`, and turns it into an absolute
    # deadline. Everything private takes the deadline. That split is what
    # keeps a nested call from re-scaling what an outer one has already
    # spent down, and what makes a partial frame's second `recv` bounded
    # by the time the caller has LEFT rather than by a fresh full budget.

    def read_frame(self, timeout: float = 15.0) -> Frame:
        """One frame. Raises `RoostError("disconnected", ...)` at a clean
        EOF — which is a legal ending, not an error, so callers that are
        draining to the close catch it."""
        return self._read_frame_until(time.monotonic() + scaled_timeout(timeout))

    def read_frames_until(
        self,
        predicate,
        timeout: float = 15.0,
        what: str = "the frame the test is waiting for",
        max_frames: int = 200_000,
    ) -> list[Frame]:
        """Read until `predicate(frame)` is true; return every frame read.

        `timeout` bounds the WHOLE call, not each frame — a tab that
        keeps producing would otherwise refresh a per-frame budget
        forever (`eventstream.recv_until`'s rule, same reason).
        """
        deadline = time.monotonic() + scaled_timeout(timeout)
        frames: list[Frame] = []
        while True:
            if time.monotonic() >= deadline:
                raise TimeoutError(f"never saw {what} (read {len(frames)} frames)")
            frame = self._read_frame_until(deadline)
            frames.append(frame)
            if len(frames) > max_frames:
                raise AssertionError(
                    f"read {len(frames)} frames without seeing {what}; the stream is "
                    "producing more than this test ever expects"
                )
            if predicate(frame):
                return frames

    def read_until_ready(self, timeout: float = 30.0) -> list[Frame]:
        """Read until the payload's readiness boundary has landed."""
        return self.read_frames_until(
            lambda _f: self.snap.ready_seen, timeout, self.snap.ready_name
        )

    def read_until_finish(self, timeout: float = 60.0) -> list[Frame]:
        """Read until the payload's end marker has landed."""
        return self.read_frames_until(
            lambda _f: self.snap.finish_seen, timeout, self.snap.end_name
        )

    def drain_to_close(self, timeout: float = 30.0, byte_cap: int | None = None) -> Ending:
        """Read to the end of the connection and say how it ended.

        Three legal endings: an `ERROR` frame, an `EXIT` frame, or a bare
        EOF — the last being what the contract accepts wherever a labeled
        final write was impossible because the peer had stopped reading.

        ERROR and EXIT are both **terminal**: the connection closes after
        either, so this proves the close rather than taking the frame's
        word for it. That the close follows is the half a client actually
        depends on — a server that wrote ERROR and kept the socket would
        leave every reader parked on a stream that is never coming back.

        `byte_cap` bounds a deliberately endless producer: past it the
        client hangs up, which is itself one of the things under test.
        """
        deadline = time.monotonic() + scaled_timeout(timeout)
        frames: list[Frame] = []
        while True:
            if time.monotonic() >= deadline:
                raise TimeoutError(
                    f"the data connection at {self.path} never ended "
                    f"(read {len(frames)} frames, {self.pty_bytes} PTY bytes)"
                )
            if byte_cap is not None and self.pty_bytes >= byte_cap:
                return Ending("cap", frames=frames)
            try:
                frame = self._read_frame_until(deadline)
            except RoostError as error:
                if error.code != "disconnected":
                    raise
                return Ending("eof", frames=frames)
            frames.append(frame)
            if frame.frame_type == FRAME_ERROR:
                body = frame.error()
                self._expect_eof_until(self._eof_deadline(deadline))
                return Ending(
                    "error",
                    code=body.get("code", ""),
                    message=body.get("message", ""),
                    frames=frames,
                )
            if frame.frame_type == FRAME_EXIT:
                final_seq, code = frame.exit()
                self._expect_eof_until(self._eof_deadline(deadline))
                return Ending(
                    "exit", final_seq=final_seq, exit_code=code, frames=frames
                )

    @staticmethod
    def _eof_deadline(deadline: float) -> float:
        """A floor under the close proof.

        The terminal frame can land with the caller's budget nearly
        spent, and "the connection did not close" is a different verdict
        from "the frame did not arrive in time" — so the proof gets its
        own small window rather than whatever milliseconds were left.
        """
        return max(deadline, time.monotonic() + scaled_timeout(5.0))

    def expect_eof(self, timeout: float = 10.0) -> None:
        """Assert the connection closes. `timeout` is a RAW budget and is
        scaled here, like every other public entry point."""
        self._expect_eof_until(time.monotonic() + scaled_timeout(timeout))

    def _expect_eof_until(self, deadline: float) -> None:
        try:
            frame = self._read_frame_until(deadline)
        except RoostError as error:
            if error.code == "disconnected":
                return
            raise
        raise AssertionError(f"expected the connection to close; got a {frame.name} frame")

    # -- writing ----------------------------------------------------------
    def send_input(self, data: bytes) -> None:
        """An `INPUT` frame. Payloads over the frame cap are the client's
        to split (the documented paste rule), so an oversized one fails
        here rather than on the wire."""
        assert len(data) <= MAX_DATA_FRAME_BYTES, (
            f"INPUT payloads are capped at {MAX_DATA_FRAME_BYTES}; split larger pastes"
        )
        self._write_frame(FRAME_INPUT, data)

    def send_resize(self, cols: int, rows: int, cell_w: int = 0, cell_h: int = 0) -> None:
        self._write_frame(FRAME_RESIZE, struct.pack("<HHHH", cols, rows, cell_w, cell_h))

    def _write_frame(self, frame_type: int, payload: bytes) -> None:
        header = struct.pack("<IB", len(payload), frame_type)
        self._sock.sendall(header + payload)

    # -- transport --------------------------------------------------------
    def _read_frame_until(self, deadline: float) -> Frame:
        """Assemble one frame by an absolute (already-scaled) deadline.

        A frame routinely needs several `recv`s — the header and the
        payload can arrive in any split — and every one of them is
        bounded by what is left of `deadline`, not by a fresh copy of the
        caller's budget. A header that arrives at the last millisecond
        must not buy its payload a whole new timeout.
        """
        while len(self._buf) < FRAME_HEADER_LEN:
            if not self._fill(deadline):
                raise RoostError("disconnected", "the session closed the data connection")
        length, frame_type = struct.unpack_from("<IB", self._buf)
        assert length <= MAX_DATA_FRAME_BYTES, (
            f"the server framed {length} bytes, past the {MAX_DATA_FRAME_BYTES} cap"
        )
        total = FRAME_HEADER_LEN + length
        while len(self._buf) < total:
            if not self._fill(deadline):
                raise RoostError("disconnected", "the session closed mid-frame")
        payload = self._buf[FRAME_HEADER_LEN:total]
        self._buf = self._buf[total:]
        frame = Frame(frame_type, payload)
        self._observe(frame)
        return frame

    def _observe(self, frame: Frame) -> None:
        """Account for one frame, and fail on the spot for anything the
        protocol says cannot happen."""
        self.frames_read += 1
        # ERROR and EXIT are terminal. Anything behind one is a server
        # that disagreed with itself about whether the stream had ended,
        # and a client that kept parsing would be building a terminal out
        # of bytes the protocol already disowned.
        if self.terminal_frame is not None:
            raise AssertionError(
                f"a {frame.name} frame arrived after the terminal "
                f"{self.terminal_frame} frame on {self.path}"
            )
        if frame.frame_type == FRAME_SNAP:
            # The end marker is the last thing the payload carries;
            # there are no bytes behind it. Under `vt` a frame here is
            # the client-side `protocol-error` the plan names, so the
            # harness treats it as the failure it is.
            if self.snap.finish_seen:
                raise AssertionError(
                    f"a SNAP frame arrived after {self.snap.end_name} closed the payload"
                )
            self.snap_frames += 1
            self.snap.feed(frame.payload)
            if self.snap.ready_seen and self.ready_at_frame is None:
                self.ready_at_frame = self.frames_read
                self.snap_frames_at_ready = self.snap_frames
        elif frame.frame_type == FRAME_PTY:
            self._reject_before_ready(frame)
            seq, data = frame.pty()
            if self.next_seq is not None and seq != self.next_seq:
                raise AssertionError(
                    f"PTY seq gap: expected {self.next_seq}, got {seq} "
                    f"(frame {self.frames_read} on {self.path})"
                )
            self.next_seq = seq + 1
            self.last_seq = seq
            self.pty_frames += 1
            self.pty_bytes += len(data)
            if self.first_pty_at_frame is None:
                self.first_pty_at_frame = self.frames_read
        elif frame.frame_type == FRAME_EXIT:
            self._reject_before_ready(frame)
            self.exit = frame.exit()
            self.terminal_frame = "EXIT"
        elif frame.frame_type == FRAME_ERROR:
            self.error = frame.error()
            self.terminal_frame = "ERROR"
        else:
            raise AssertionError(f"unknown server frame type 0x{frame.frame_type:02x}")

    def _reject_before_ready(self, frame: Frame) -> None:
        """Nothing but payload bytes precedes readiness, in snapshot mode.

        The boundary is the point at which the client first has a
        terminal, and a `PTY` frame ahead of it is a frame with nowhere
        to go — the client would have to queue it itself, which is the
        queue the server holds on its behalf (architecture §4.3 step 3,
        "queues the rest"; `attach.rs`'s `holding = sent <
        self.ready_end || terminator_due`). So the prefix goes out whole
        and live traffic starts flowing behind it. Under `vt` the prefix
        is the whole payload, which is why the terminator is what a
        `PTY` frame must wait for there.

        Resume streams are exempt and must be: they carry no payload at
        all, so `ready_seen` is never true and every frame on them is a
        PTY frame by construction. `ERROR` is exempt in both modes — it
        can end a connection at any point, including during the prefix.
        """
        if self.snapshot_mode and not self.snap.ready_seen:
            raise AssertionError(
                f"a {frame.name} frame arrived before {self.snap.ready_name} "
                f"(after {self.snap_frames} SNAP frames, {self.snap.total_bytes} SNAP bytes)"
            )

    def _readline(self, deadline: float) -> str:
        while b"\n" not in self._buf:
            if not self._fill(deadline):
                raise RoostError("disconnected", "the session closed before its handshake reply")
        line, self._buf = self._buf.split(b"\n", 1)
        return line.decode()

    def _read_to_eof(self, deadline: float) -> bytes:
        """Everything left on the socket, up to the close. Bounded: a
        server that refuses a handshake and then keeps the connection is
        itself a failure, and this must report it rather than hang."""
        try:
            while self._fill(deadline):
                pass
        except TimeoutError as error:
            raise AssertionError(
                f"the session refused the handshake but never closed the connection "
                f"(it wrote {len(self._buf)} more bytes)"
            ) from error
        rest, self._buf = self._buf, b""
        return rest

    def _fill(self, deadline: float) -> bool:
        """One `recv`, bounded by what is left of `deadline`.

        The remaining time is recomputed here rather than passed in, so
        every read in a multi-`recv` assembly is charged against the same
        wall clock. False at EOF; the caller decides whether that is an
        ending or a truncation.
        """
        if self._eof:
            return False
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError(f"no data-plane bytes from {self.path} within its budget")
        self._sock.settimeout(remaining)
        try:
            chunk = self._sock.recv(1 << 16)
        except TimeoutError as error:
            raise TimeoutError(
                f"no data-plane bytes from {self.path} within its budget"
            ) from error
        if not chunk:
            self._eof = True
            return False
        self._buf += chunk
        return True
