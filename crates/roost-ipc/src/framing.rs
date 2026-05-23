//! Newline-delimited JSON framing over async byte streams.
//!
//! One JSON object per line. Max line length is [`crate::MAX_FRAME_BYTES`]
//! (16 MiB) — overflowing lines fail with [`crate::Error::FrameTooLarge`]
//! and the connection should be torn down by the caller.
//!
//! The reader is generic over any `AsyncRead + Unpin` (tokio
//! `UnixStream` and `&[u8]` test fixtures both qualify). The writer
//! mirrors that against any `AsyncWrite + Unpin`.

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::{Error, MAX_FRAME_BYTES};

/// Reader-side framer. Owns an internal buffer; call [`Self::read_line`]
/// in a loop to consume frames.
pub struct FrameReader<R> {
    inner: R,
    /// Bytes read but not yet handed out as a complete line.
    pending: Vec<u8>,
    /// Read-buffer scratch.
    scratch: Vec<u8>,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            pending: Vec::new(),
            // 64 KiB is a comfortable default for typical tab.write
            // payloads; the buffer will grow if needed.
            scratch: vec![0u8; 64 * 1024],
        }
    }

    /// Read one newline-delimited line, sans terminator. Returns
    /// `Ok(None)` on clean EOF (no bytes pending); returns
    /// `Err(UnexpectedEof)` if the connection closes mid-frame.
    pub async fn read_line(&mut self) -> Result<Option<Vec<u8>>, Error> {
        use tokio::io::AsyncReadExt;

        loop {
            // Look for a newline in what we already have.
            if let Some(pos) = self.pending.iter().position(|&b| b == b'\n') {
                let mut line = self.pending.split_off(pos + 1);
                std::mem::swap(&mut line, &mut self.pending);
                // `line` now holds everything up to and including `\n`;
                // `self.pending` holds the rest. Drop the trailing `\n`.
                line.pop();
                if line.len() > MAX_FRAME_BYTES {
                    return Err(Error::FrameTooLarge);
                }
                return Ok(Some(line));
            }

            // Defend against an unbounded line: if `pending` already
            // exceeds the cap with no `\n` in sight, give up. (We allow
            // exactly MAX_FRAME_BYTES, no more — the `>` here is on the
            // total pre-newline byte count.)
            if self.pending.len() > MAX_FRAME_BYTES {
                return Err(Error::FrameTooLarge);
            }

            // Pull more from the underlying reader.
            let n = self.inner.read(&mut self.scratch).await?;
            if n == 0 {
                if self.pending.is_empty() {
                    return Ok(None);
                } else {
                    return Err(Error::UnexpectedEof);
                }
            }
            self.pending.extend_from_slice(&self.scratch[..n]);
        }
    }
}

/// Write a single frame: the `bytes` followed by `\n`. The payload
/// must not exceed [`crate::MAX_FRAME_BYTES`]. Encoders should produce
/// a single newline at the end; nothing in this function strips
/// embedded newlines, but callers using `serde_json::to_vec` will
/// never embed a literal `\n` since `serde_json` always escapes it.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, bytes: &[u8]) -> Result<(), Error> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(Error::FrameTooLarge);
    }
    w.write_all(bytes).await?;
    w.write_all(b"\n").await?;
    w.flush().await?;
    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    async fn read_all(reader: &mut FrameReader<&[u8]>) -> Vec<Vec<u8>> {
        let mut out = vec![];
        while let Some(line) = reader.read_line().await.expect("read") {
            out.push(line);
        }
        out
    }

    #[tokio::test]
    async fn reads_single_frame() {
        let buf = b"hello\n".as_slice();
        let mut r = FrameReader::new(buf);
        let lines = read_all(&mut r).await;
        assert_eq!(lines, vec![b"hello".to_vec()]);
    }

    #[tokio::test]
    async fn reads_multiple_frames_in_one_read() {
        let buf = b"one\ntwo\nthree\n".as_slice();
        let mut r = FrameReader::new(buf);
        let lines = read_all(&mut r).await;
        assert_eq!(
            lines,
            vec![b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]
        );
    }

    #[tokio::test]
    async fn empty_stream_yields_none() {
        let buf: &[u8] = b"";
        let mut r = FrameReader::new(buf);
        assert!(r.read_line().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn unterminated_line_is_unexpected_eof() {
        let buf = b"no newline here".as_slice();
        let mut r = FrameReader::new(buf);
        match r.read_line().await {
            Err(Error::UnexpectedEof) => {}
            other => panic!("expected UnexpectedEof, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn split_reads_are_concatenated() {
        // Simulate two split reads via `AsyncReadExt::chain`, which
        // serves the two halves sequentially. Pins the documented
        // invariant that a frame split across syscalls is reassembled.
        let part_a = b"hel".as_slice();
        let part_b = b"lo\nworld\n".as_slice();
        let chained = part_a.chain(part_b);
        let mut r = FrameReader::new(chained);
        let lines = vec![
            r.read_line().await.unwrap().unwrap(),
            r.read_line().await.unwrap().unwrap(),
        ];
        assert_eq!(lines, vec![b"hello".to_vec(), b"world".to_vec()]);
    }

    #[tokio::test]
    async fn frame_at_max_bytes_succeeds() {
        // Build exactly MAX_FRAME_BYTES of data + a newline. Use a
        // 1 MiB cap in tests via a custom path? No — we use the real
        // cap. 16 MiB allocates plenty of RAM but is fine for a single
        // test pass.
        let payload = vec![b'a'; MAX_FRAME_BYTES];
        let mut buf = payload.clone();
        buf.push(b'\n');
        let mut r = FrameReader::new(buf.as_slice());
        let line = r.read_line().await.unwrap().unwrap();
        assert_eq!(line.len(), MAX_FRAME_BYTES);
    }

    #[tokio::test]
    async fn frame_over_max_bytes_is_rejected() {
        let payload = vec![b'a'; MAX_FRAME_BYTES + 1];
        let mut buf = payload.clone();
        buf.push(b'\n');
        let mut r = FrameReader::new(buf.as_slice());
        match r.read_line().await {
            Err(Error::FrameTooLarge) => {}
            other => panic!("expected FrameTooLarge, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn write_frame_appends_newline() {
        let mut buf = Vec::<u8>::new();
        write_frame(&mut buf, b"hello").await.unwrap();
        assert_eq!(buf, b"hello\n");
    }

    #[tokio::test]
    async fn write_frame_rejects_oversized_payload() {
        let oversized = vec![b'a'; MAX_FRAME_BYTES + 1];
        let mut buf = Vec::<u8>::new();
        match write_frame(&mut buf, &oversized).await {
            Err(Error::FrameTooLarge) => {}
            other => panic!("expected FrameTooLarge, got {other:?}"),
        }
    }
}
