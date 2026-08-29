//! Binary framing for host-session attach data connections
//! (`docs/reference/ipc.md`, data plane).
//!
//! A data connection starts as newline-JSON — the attach handshake and
//! its reply — and then turns binary: the 8-byte [`PREAMBLE`], followed
//! by length-prefixed frames in both directions.
//!
//! ```text
//! frame := u32-LE payload length | u8 type | payload
//! ```
//!
//! Everything here is transport: the reader hands back a type byte and
//! a payload, and enforces exactly one rule — the [`MAX_DATA_FRAME_BYTES`]
//! cap, which has to live here because it bounds the allocation made
//! before any caller sees the frame. Per-type payload validation (`PTY`
//! ≥ 9 bytes, `EXIT` == 12, `RESIZE` == 8, which types may be empty,
//! which types may arrive in which direction) belongs to the endpoint
//! that knows the protocol state, not to the framer.
//!
//! Shared by the server, the Rust integration client, and the tests, so
//! there is exactly one implementation of the byte layout.

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::Error;

/// Written by the server once the handshake is accepted, immediately
/// before the first frame. The trailing `2` is
/// [`crate::messages::SESSION_PROTOCOL_VERSION`]'s generation of the
/// data plane: a client that reads a different magic has negotiated
/// with a host it cannot talk to and must not try to parse what
/// follows.
pub const PREAMBLE: [u8; 8] = *b"ROOSTDP2";

/// Header bytes ahead of every payload: 4 for the length, 1 for the type.
pub const FRAME_HEADER_LEN: usize = 5;

/// Largest payload a single frame may carry, both directions. A client
/// with a bigger paste splits it across `INPUT` frames; a server with a
/// bigger snapshot record splits it across `SNAP` frames (the snapshot
/// stream is a byte stream, not a record-aligned one).
pub const MAX_DATA_FRAME_BYTES: usize = 1024 * 1024;

/// Server → client: the next bytes of the encoded snapshot stream. The
/// only type for which a zero-length payload is meaningful.
pub const FRAME_SNAP: u8 = 0x01;
/// Server → client: `u64-LE seq | raw PTY bytes`.
pub const FRAME_PTY: u8 = 0x02;
/// Server → client: `u64-LE final_seq | i32-LE exit code`. Always the
/// last frame on a connection that sees it.
pub const FRAME_EXIT: u8 = 0x03;
/// Server → client: a JSON `{code, message}` diagnostic. The connection
/// closes after it.
pub const FRAME_ERROR: u8 = 0x0F;
/// Client → server: raw input bytes, ordered and unacknowledged.
pub const FRAME_INPUT: u8 = 0x11;
/// Client → server: `u16-LE cols | rows | cell_w_px | cell_h_px`.
pub const FRAME_RESIZE: u8 = 0x12;

/// One decoded frame. The type byte is deliberately not an enum: an
/// unknown type is a protocol error the *endpoint* answers (with an
/// `ERROR` frame naming it), not a decode failure that would cost the
/// framer the ability to report which byte it saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataFrame {
    pub frame_type: u8,
    pub payload: Vec<u8>,
}

/// Write the 8-byte magic that opens the binary half of a connection.
pub async fn write_preamble<W: AsyncWrite + Unpin>(w: &mut W) -> Result<(), Error> {
    w.write_all(&PREAMBLE).await?;
    w.flush().await?;
    Ok(())
}

/// Write one frame. Rejects an oversized payload rather than emitting a
/// header the peer is required to treat as fatal.
pub async fn write_data_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    frame_type: u8,
    payload: &[u8],
) -> Result<(), Error> {
    if payload.len() > MAX_DATA_FRAME_BYTES {
        return Err(Error::DataFrameTooLarge);
    }
    let mut header = [0u8; FRAME_HEADER_LEN];
    // `as u32` is safe under the cap checked above.
    header[..4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    header[4] = frame_type;
    w.write_all(&header).await?;
    w.write_all(payload).await?;
    w.flush().await?;
    Ok(())
}

/// Reader over the binary half of a data connection.
///
/// Constructed with the bytes a preceding line reader had already
/// buffered ("residue"): the handshake line and the first binary bytes
/// can arrive in one `read`, so those bytes must be consumed *before*
/// the socket is touched again or the stream loses its head.
pub struct DataFrameReader<R> {
    inner: R,
    /// Bytes read but not yet handed out, from `start` on. Residue
    /// first.
    pending: Vec<u8>,
    /// How much of `pending` the frames handed out already consumed.
    /// Reclaimed lazily, only when the next read needs the room, so a
    /// buffer holding a dozen small PTY frames is memmoved once per
    /// read instead of once per frame.
    start: usize,
    scratch: Vec<u8>,
}

impl<R: AsyncRead + Unpin> DataFrameReader<R> {
    pub fn new(inner: R, residue: Vec<u8>) -> Self {
        Self {
            inner,
            pending: residue,
            start: 0,
            // Comfortably above a PTY read chunk; grows only if a
            // caller swaps in a bigger reader.
            scratch: vec![0u8; 64 * 1024],
        }
    }

    /// Read and verify the 8-byte [`PREAMBLE`].
    pub async fn read_preamble(&mut self) -> Result<(), Error> {
        if !self.fill_at_least(PREAMBLE.len()).await? {
            return Err(Error::UnexpectedEof);
        }
        if self.buffered()[..PREAMBLE.len()] != PREAMBLE {
            return Err(Error::BadPreamble);
        }
        self.start += PREAMBLE.len();
        Ok(())
    }

    /// The next frame, or `Ok(None)` on a clean EOF at a frame
    /// boundary. An EOF part-way through a header or payload is
    /// [`Error::UnexpectedEof`] — the distinction is what tells a
    /// client "the peer finished" from "the peer died mid-frame".
    pub async fn next_frame(&mut self) -> Result<Option<DataFrame>, Error> {
        if !self.fill_at_least(FRAME_HEADER_LEN).await? {
            return if self.buffered().is_empty() {
                Ok(None)
            } else {
                Err(Error::UnexpectedEof)
            };
        }
        let header = self.buffered();
        let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let frame_type = header[4];
        // Checked before the fill so a bogus length can never make this
        // buffer the peer's whole stream waiting for bytes it will
        // refuse anyway.
        if len > MAX_DATA_FRAME_BYTES {
            return Err(Error::DataFrameTooLarge);
        }
        if !self.fill_at_least(FRAME_HEADER_LEN + len).await? {
            return Err(Error::UnexpectedEof);
        }
        let payload = self.buffered()[FRAME_HEADER_LEN..FRAME_HEADER_LEN + len].to_vec();
        self.start += FRAME_HEADER_LEN + len;
        Ok(Some(DataFrame {
            frame_type,
            payload,
        }))
    }

    /// The underlying reader plus whatever is still buffered — the
    /// mirror of [`crate::framing::FrameReader::into_parts`], for a
    /// caller that hands the connection on again.
    pub fn into_parts(mut self) -> (R, Vec<u8>) {
        self.reclaim();
        (self.inner, self.pending)
    }

    /// What has been read but not handed out yet.
    fn buffered(&self) -> &[u8] {
        &self.pending[self.start..]
    }

    /// Drop the consumed prefix.
    fn reclaim(&mut self) {
        if self.start > 0 {
            self.pending.drain(..self.start);
            self.start = 0;
        }
    }

    /// `Ok(false)` means EOF arrived before `n` bytes were available.
    async fn fill_at_least(&mut self, n: usize) -> Result<bool, Error> {
        use tokio::io::AsyncReadExt;

        while self.buffered().len() < n {
            self.reclaim();
            let read = self.inner.read(&mut self.scratch).await?;
            if read == 0 {
                return Ok(false);
            }
            self.pending.extend_from_slice(&self.scratch[..read]);
        }
        Ok(true)
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn reader(bytes: &[u8]) -> DataFrameReader<&[u8]> {
        DataFrameReader::new(bytes, Vec::new())
    }

    async fn encode(frames: &[(u8, Vec<u8>)]) -> Vec<u8> {
        let mut buf = Vec::new();
        for (frame_type, payload) in frames {
            write_data_frame(&mut buf, *frame_type, payload)
                .await
                .expect("write");
        }
        buf
    }

    #[tokio::test]
    async fn preamble_is_the_pinned_magic() {
        assert_eq!(&PREAMBLE, b"ROOSTDP2");
        let mut buf = Vec::new();
        write_preamble(&mut buf).await.unwrap();
        assert_eq!(buf, b"ROOSTDP2");

        let mut r = reader(&buf);
        r.read_preamble().await.expect("preamble");
        assert!(r.next_frame().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_wrong_preamble_is_rejected() {
        let mut r = reader(b"ROOSTDP1");
        match r.read_preamble().await {
            Err(Error::BadPreamble) => {}
            other => panic!("expected BadPreamble, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_short_preamble_is_unexpected_eof() {
        let mut r = reader(b"ROOST");
        match r.read_preamble().await {
            Err(Error::UnexpectedEof) => {}
            other => panic!("expected UnexpectedEof, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn every_frame_type_round_trips() {
        let frames: Vec<(u8, Vec<u8>)> = vec![
            (FRAME_SNAP, b"GHOSTSNP\x00\x01".to_vec()),
            (FRAME_PTY, {
                let mut p = 42u64.to_le_bytes().to_vec();
                p.extend_from_slice(b"hello world");
                p
            }),
            (FRAME_EXIT, {
                let mut p = 99u64.to_le_bytes().to_vec();
                p.extend_from_slice(&(-1i32).to_le_bytes());
                p
            }),
            (
                FRAME_ERROR,
                br#"{"code":"desync","message":"lagged"}"#.to_vec(),
            ),
            (FRAME_INPUT, b"ls -la\r".to_vec()),
            (FRAME_RESIZE, {
                let mut p = Vec::new();
                for v in [120u16, 40, 9, 18] {
                    p.extend_from_slice(&v.to_le_bytes());
                }
                p
            }),
            // Zero-length is legal at the framing layer; which types may
            // use it is the endpoint's rule.
            (FRAME_SNAP, Vec::new()),
        ];

        let wire = encode(&frames).await;
        let mut r = reader(&wire);
        for (frame_type, payload) in &frames {
            let got = r.next_frame().await.expect("read").expect("frame");
            assert_eq!(got.frame_type, *frame_type);
            assert_eq!(&got.payload, payload);
        }
        assert!(r.next_frame().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn the_header_is_little_endian_length_then_type() {
        let mut buf = Vec::new();
        write_data_frame(&mut buf, FRAME_PTY, &[0xAA, 0xBB, 0xCC])
            .await
            .unwrap();
        assert_eq!(buf, vec![3, 0, 0, 0, FRAME_PTY, 0xAA, 0xBB, 0xCC]);
    }

    /// The wire is a stream: a header, a length prefix, and a payload
    /// can each be split across any number of reads. Feeding one byte
    /// per read is the strongest form of that.
    #[tokio::test]
    async fn frames_reassemble_across_byte_at_a_time_reads() {
        let frames: Vec<(u8, Vec<u8>)> = vec![
            (FRAME_SNAP, vec![7u8; 300]),
            (FRAME_PTY, {
                let mut p = 1u64.to_le_bytes().to_vec();
                p.extend_from_slice(b"x");
                p
            }),
            (FRAME_EXIT, vec![0u8; 12]),
        ];
        let wire = encode(&frames).await;

        // A one-byte scratch buffer makes every `read` yield exactly one
        // byte, so every boundary (length, type, payload) is crossed
        // mid-read.
        let mut r = DataFrameReader {
            inner: wire.as_slice(),
            pending: Vec::new(),
            start: 0,
            scratch: vec![0u8; 1],
        };
        for (frame_type, payload) in &frames {
            let got = r.next_frame().await.expect("read").expect("frame");
            assert_eq!(got.frame_type, *frame_type);
            assert_eq!(&got.payload, payload);
        }
        assert!(r.next_frame().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn residue_is_consumed_before_the_reader() {
        // What the line reader had already buffered past the handshake:
        // the preamble and the head of the first frame.
        let wire = encode(&[(FRAME_SNAP, b"abcdef".to_vec())]).await;
        let mut residue = PREAMBLE.to_vec();
        residue.extend_from_slice(&wire[..3]);

        let mut r = DataFrameReader::new(&wire[3..], residue);
        r.read_preamble().await.expect("preamble from residue");
        let frame = r.next_frame().await.expect("read").expect("frame");
        assert_eq!(frame.frame_type, FRAME_SNAP);
        assert_eq!(frame.payload, b"abcdef");
        assert!(r.next_frame().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn residue_alone_can_carry_whole_frames() {
        let wire = encode(&[(FRAME_INPUT, b"a".to_vec()), (FRAME_INPUT, b"bc".to_vec())]).await;
        let empty: &[u8] = b"";
        let mut r = DataFrameReader::new(empty, wire);
        assert_eq!(r.next_frame().await.unwrap().unwrap().payload, b"a");
        assert_eq!(r.next_frame().await.unwrap().unwrap().payload, b"bc");
        assert!(r.next_frame().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_frame_at_the_cap_is_accepted() {
        let payload = vec![0x5Au8; MAX_DATA_FRAME_BYTES];
        let wire = encode(&[(FRAME_SNAP, payload.clone())]).await;
        let mut r = reader(&wire);
        let got = r.next_frame().await.expect("read").expect("frame");
        assert_eq!(got.payload.len(), MAX_DATA_FRAME_BYTES);
    }

    #[tokio::test]
    async fn an_over_cap_length_is_rejected_without_reading_the_payload() {
        // Header only — a reader that tried to buffer the claimed
        // payload would block here instead of erroring.
        let mut wire = ((MAX_DATA_FRAME_BYTES + 1) as u32).to_le_bytes().to_vec();
        wire.push(FRAME_SNAP);
        let mut r = reader(&wire);
        match r.next_frame().await {
            Err(Error::DataFrameTooLarge) => {}
            other => panic!("expected DataFrameTooLarge, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn writing_an_over_cap_payload_is_refused() {
        let mut buf = Vec::new();
        let payload = vec![0u8; MAX_DATA_FRAME_BYTES + 1];
        match write_data_frame(&mut buf, FRAME_INPUT, &payload).await {
            Err(Error::DataFrameTooLarge) => {}
            other => panic!("expected DataFrameTooLarge, got {other:?}"),
        }
        assert!(buf.is_empty(), "nothing may reach the wire");
    }

    #[tokio::test]
    async fn a_truncated_header_is_unexpected_eof() {
        let mut r = reader(&[3, 0, 0]);
        match r.next_frame().await {
            Err(Error::UnexpectedEof) => {}
            other => panic!("expected UnexpectedEof, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_truncated_payload_is_unexpected_eof() {
        let wire = encode(&[(FRAME_PTY, vec![1u8; 32])]).await;
        let mut r = reader(&wire[..wire.len() - 1]);
        match r.next_frame().await {
            Err(Error::UnexpectedEof) => {}
            other => panic!("expected UnexpectedEof, got {other:?}"),
        }
    }

    /// An unknown type is transport-legal: the framer hands it up so
    /// the endpoint can answer with an `ERROR` naming the byte.
    #[tokio::test]
    async fn an_unknown_type_byte_decodes_and_is_left_to_the_caller() {
        let wire = encode(&[(0x7E, b"?".to_vec())]).await;
        let mut r = reader(&wire);
        let got = r.next_frame().await.unwrap().unwrap();
        assert_eq!(got.frame_type, 0x7E);
        assert_eq!(got.payload, b"?");
    }

    #[tokio::test]
    async fn into_parts_hands_back_what_is_still_buffered() {
        let wire = encode(&[
            (FRAME_INPUT, b"one".to_vec()),
            (FRAME_INPUT, b"two".to_vec()),
        ])
        .await;
        let mut r = reader(&wire);
        assert_eq!(r.next_frame().await.unwrap().unwrap().payload, b"one");
        let (rest, residue) = r.into_parts();
        let mut again = DataFrameReader::new(rest, residue);
        assert_eq!(again.next_frame().await.unwrap().unwrap().payload, b"two");
    }
}
