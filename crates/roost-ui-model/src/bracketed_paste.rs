//! Toolkit-neutral bracketed-paste framing for pasted / dropped payloads.
//!
//! Mirrors `mac/Sources/Roost/BracketedPaste.swift` (`wrapBracketedPaste`);
//! the two implementations share unit-test vectors so they stay
//! byte-identical, following the `shell_escape` precedent.

const START: &[u8] = b"\x1b[200~";
const END: &[u8] = b"\x1b[201~";
const MARKER_LEN: usize = START.len();

/// Frame `text` for the PTY.
///
/// When `bracketed` (DECSET 2004 active) the payload is wrapped in
/// `ESC[200~` … `ESC[201~` and every embedded `ESC[200~` / `ESC[201~` is
/// removed first — a clipboard carrying `ESC[201~rm -rf /` would otherwise
/// close the region early and the tail would reach the shell as typed input.
/// Removal (rather than escaping) matches the upstream terminal convention:
/// there is no in-band way to quote a marker, so it is neutralized.
///
/// Without `bracketed` the bytes pass through unchanged — the receiving
/// program is reading raw input, so there is no region to break out of.
/// Empty input yields no bytes so callers never emit a bare
/// `ESC[200~ESC[201~`.
pub fn wrap(text: &str, bracketed: bool) -> Vec<u8> {
    if text.is_empty() {
        return Vec::new();
    }
    if !bracketed {
        return text.as_bytes().to_vec();
    }
    let mut out = Vec::with_capacity(text.len() + START.len() + END.len());
    out.extend_from_slice(START);
    for &byte in text.as_bytes() {
        out.push(byte);
        // Match on the *output* tail, not the input: dropping one marker can
        // splice its neighbours into a fresh one (`ESC[20` + `ESC[200~` +
        // `0~`), which a single input-side pass would let through. The
        // `>= MARKER_LEN` floor keeps the opening `START` out of the window.
        let tail = out.len().saturating_sub(MARKER_LEN);
        if tail >= MARKER_LEN && (&out[tail..] == START || &out[tail..] == END) {
            out.truncate(tail);
        }
    }
    out.extend_from_slice(END);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_yields_no_bytes() {
        assert!(wrap("", false).is_empty());
        assert!(wrap("", true).is_empty());
    }

    #[test]
    fn passthrough_without_bracketed_mode() {
        assert_eq!(wrap("hello\n", false), b"hello\n");
        // No 2004, no region to break: the bytes are delivered verbatim.
        assert_eq!(wrap("a\x1b[201~b", false), b"a\x1b[201~b");
    }

    #[test]
    fn plain_payload_is_wrapped_once() {
        assert_eq!(wrap("hello\n", true), b"\x1b[200~hello\n\x1b[201~");
    }

    #[test]
    fn embedded_end_marker_is_removed() {
        assert_eq!(
            wrap("\x1b[201~rm -rf /\n", true),
            b"\x1b[200~rm -rf /\n\x1b[201~"
        );
    }

    #[test]
    fn embedded_start_marker_is_removed() {
        assert_eq!(wrap("a\x1b[200~b", true), b"\x1b[200~ab\x1b[201~");
    }

    /// Removal is re-checked against the output, so halves left adjacent by an
    /// earlier removal cannot re-form a marker.
    #[test]
    fn removal_cannot_splice_a_new_marker() {
        assert_eq!(wrap("x\x1b[20\x1b[200~0~y", true), b"\x1b[200~xy\x1b[201~");
    }

    /// Only the contiguous six-byte sequence is a marker; a truncated prefix
    /// is ordinary payload.
    #[test]
    fn partial_marker_is_preserved() {
        assert_eq!(wrap("\x1b[201", true), b"\x1b[200~\x1b[201\x1b[201~");
    }

    #[test]
    fn utf8_around_removals_is_preserved() {
        assert_eq!(
            wrap("图\x1b[201~片", true),
            "\x1b[200~图片\x1b[201~".as_bytes()
        );
    }
}

