// Bracketed-paste framing for pasted / dropped payloads.
//
// Mirrors `crates/roost-ui-model/src/bracketed_paste.rs` (`bracketed_paste::wrap`);
// the two implementations share unit-test vectors so they stay byte-identical,
// following the `ShellEscape` precedent.

import Foundation

private let bracketedPasteStart: [UInt8] = [0x1b, 0x5b, 0x32, 0x30, 0x30, 0x7e]
private let bracketedPasteEnd: [UInt8] = [0x1b, 0x5b, 0x32, 0x30, 0x31, 0x7e]

/// Frame `payload` for the PTY.
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
func wrapBracketedPaste(_ payload: Data, bracketed: Bool) -> Data {
    if payload.isEmpty { return Data() }
    guard bracketed else { return payload }
    let markerLen = bracketedPasteStart.count
    var out = [UInt8]()
    out.reserveCapacity(payload.count + markerLen * 2)
    out.append(contentsOf: bracketedPasteStart)
    for byte in payload {
        out.append(byte)
        // Match on the *output* tail, not the input: dropping one marker can
        // splice its neighbours into a fresh one (`ESC[20` + `ESC[200~` +
        // `0~`), which a single input-side pass would let through. The
        // doubled-length floor keeps the opening marker out of the window.
        guard out.count >= markerLen * 2 else { continue }
        let tail = out.suffix(markerLen)
        if tail.elementsEqual(bracketedPasteStart) || tail.elementsEqual(bracketedPasteEnd) {
            out.removeLast(markerLen)
        }
    }
    out.append(contentsOf: bracketedPasteEnd)
    return Data(out)
}
