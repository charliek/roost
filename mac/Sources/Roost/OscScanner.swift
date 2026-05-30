// Streaming OSC scanner — Phase 6a P6 (UI side).
//
// Swift mirror of `crates/roost-core/src/osc.rs` (the P4 daemon-side
// scanner). The Mac UI runs its own copy of the state machine
// because the UI is what owns the PTY-byte stream: it receives
// PtyOutput bytes via StreamPty, writes them to libghostty for
// rendering, AND scans them for OSCs that should be reported up
// to the daemon via ReportOsc.
//
// libghostty exposes its own OSC parser (`ghostty_osc_*` in
// `third_party/ghostty/out/include/ghostty/vt/osc.h`), but using
// it from Swift would require driving the parser byte-by-byte
// alongside `ghostty_terminal_vt_write`, plus a separate
// terminator-detection state. Porting the ~80-line P4 state
// machine is simpler and keeps the UI parser identical in shape
// to the daemon's for easier diffing.
//
// Out of scope (matches P4):
//   * OSC 99 (id'd notification).
//   * OSC 10/11/12 response synthesis — emit events, caller
//     decides whether to synthesize replies.
//
// Bodies cap at 8KB to match P4's `MAX_BODY`.

import Foundation

/// Maximum number of body bytes the scanner buffers before
/// truncating. 1 MiB accommodates realistic OSC 52 clipboard payloads
/// while still bounding the scanner against a misbehaving emitter.
/// Mirrors `crates/roost-osc/src/lib.rs::MAX_BODY` 1:1.
private let maxBody = 1024 * 1024

/// One parsed-out OSC event. Returned in order by
/// `OscScanner.feed`.
enum OscClipboardTarget: Equatable {
    case system
    case selection
}

enum OscEvent: Equatable {
    case title(String)
    case pwd(String)
    case notification(title: String, body: String)
    case colorQuery(UInt8)  // 10, 11, or 12
    case commandMark(String)  // OSC 133 prompt/command mark body (A/B/C/D[;exit])
    /// OSC 52 program-initiated clipboard write. The body's base64
    /// payload has already been decoded; `text` is plain UTF-8.
    /// Read requests (`Pc == "?"`) are dropped at parse time —
    /// phase 1 is write-only.
    case clipboard(target: OscClipboardTarget, text: String)
    /// OSC 22 — set the mouse pointer shape by W3C/CSS cursor name
    /// (`pointer`, `default`, `text`, `crosshair`, `grab`, `grabbing`,
    /// `not-allowed`, `n/s/e/w-resize`, …). Empty name and unknown
    /// names both pass through; the UI maps them to the platform
    /// default cursor. Strix uses `pointer` for the divider grab and
    /// `default` to reset.
    case mouseShape(String)

    /// Maps a parsed event to the (osc_command, payload) pair the
    /// `ReportOsc` RPC expects. Used by `TerminalView.appendBytes`
    /// to bridge scanner output into the existing gRPC surface.
    /// Returns `nil` for events that are pure UI actions (e.g.
    /// `.clipboard`) and don't belong on the daemon-side dispatch.
    var asReport: (UInt32, String)? {
        switch self {
        case .title(let s):
            // OSC 0 is the conservative pick for sending titles
            // up to the daemon — its dispatch (P5) treats 0/1/2
            // the same way. Re-classifying as 1 vs 2 in the UI
            // would require tracking the original prefix; not
            // worth the bookkeeping for the same result.
            return (0, s)
        case .pwd(let p):
            // Daemon's OSC 7 dispatch expects either the raw
            // file:// URI (it'll parse) OR the already-decoded
            // path (it'll pass through). Send the decoded path
            // since we already paid the parse cost here.
            return (7, p)
        case .notification(let title, let body):
            // Daemon's OSC 9 dispatch treats body as a
            // title-only notification; OSC 777 dispatch parses
            // `notify;<title>;<body>` and sends the full pair.
            // Use OSC 777 for full fidelity when body is set;
            // OSC 9 for title-only.
            if body.isEmpty {
                return (9, title)
            } else {
                return (777, "notify;\(title);\(body)")
            }
        case .colorQuery(let n):
            // No daemon-side handling for color queries; just
            // emit the command number with empty payload so the
            // dispatch records it.
            return (UInt32(n), "?")
        case .commandMark(let s):
            // OSC 133 prompt/command mark; pass the body through to
            // applyOSC, which maps it to tab state (P4b).
            return (133, s)
        case .clipboard:
            // OSC 52 is a UI-only action — the daemon doesn't track
            // pasteboard state, so don't forward.
            return nil
        case .mouseShape:
            // OSC 22 is a UI-only action — only the Mac/GTK renderer
            // owns the OS cursor; the daemon has no concept of
            // pointer shape.
            return nil
        }
    }
}

/// State the byte-by-byte parser cycles through. Matches the P4
/// Rust enum 1:1.
private enum ScanState {
    case outside
    case esc      // saw ESC, waiting for ']'
    case prefix   // collecting <number> before ';'
    case body     // collecting body before BEL or ESC '\\'
    case bodyEsc  // saw ESC in body, expecting '\\'
}

/// Stateful OSC byte-stream scanner. Not safe for concurrent use;
/// each TerminalView gets its own.
final class OscScanner {
    private var state: ScanState = .outside
    private var num: String = ""
    /// Body is accumulated as raw bytes so multi-byte UTF-8
    /// sequences (emoji, CJK, anything outside ASCII) round-trip
    /// intact. The earlier implementation appended each byte as
    /// `Character(UnicodeScalar(b))`, which interprets the byte
    /// as a Latin-1 codepoint — `0xF0 0x9F 0x9F 0xA2` (🟢) became
    /// `"ð¢"` (Latin-1 ð + Latin-1 control + Latin-1 control +
    /// Latin-1 ¢) in tab titles. Bytes go in as bytes; UTF-8
    /// decode happens at dispatch time when we hand a String to
    /// downstream consumers.
    private var bodyBytes: [UInt8] = []
    /// `true` if the current OSC body grew past `maxBody` and the
    /// trailing bytes were dropped. OSC 52 dispatch refuses to emit
    /// when this is set — a partial base64 decode would silently
    /// write the wrong text to the user's clipboard. Reset on each
    /// new OSC. Mirrors the Rust scanner's `body_truncated`.
    private var bodyTruncated: Bool = false
    private var pending: [OscEvent] = []

    /// Feed a slice of PTY bytes. Returns OSC events parsed out
    /// in feed order. The caller is responsible for ALSO writing
    /// the bytes through to libghostty / the renderer — the
    /// scanner is purely additive, observing the stream.
    func feed(_ bytes: Data) -> [OscEvent] {
        pending.removeAll(keepingCapacity: true)
        for b in bytes {
            step(b)
        }
        let out = pending
        pending.removeAll(keepingCapacity: true)
        return out
    }

    private func step(_ b: UInt8) {
        switch state {
        case .outside:
            if b == 0x1B {
                state = .esc
            }
        case .esc:
            if b == UInt8(ascii: "]") {
                state = .prefix
                num.removeAll(keepingCapacity: true)
                bodyBytes.removeAll(keepingCapacity: true)
                bodyTruncated = false
            } else if b == 0x1B {
                // ESC ESC: stay in esc.
            } else {
                state = .outside
            }
        case .prefix:
            switch b {
            case UInt8(ascii: ";"):
                state = .body
            case 0x07:
                dispatch()
                state = .outside
            case 0x1B:
                state = .bodyEsc
            case UInt8(ascii: "0")...UInt8(ascii: "9"),
                UInt8(ascii: "a")...UInt8(ascii: "z"),
                UInt8(ascii: "A")...UInt8(ascii: "Z"):
                if num.count < 8 {
                    num.append(Character(UnicodeScalar(b)))
                }
            default:
                state = .outside
            }
        case .body:
            switch b {
            case 0x07:
                dispatch()
                state = .outside
            case 0x1B:
                state = .bodyEsc
            default:
                if bodyBytes.count < maxBody {
                    bodyBytes.append(b)
                } else {
                    bodyTruncated = true
                }
            }
        case .bodyEsc:
            if b == UInt8(ascii: "\\") {
                dispatch()
                state = .outside
                return
            }
            // ESC followed by non-\\ aborts the sequence. Re-feed
            // the byte so an ESC starting a fresh OSC isn't lost.
            state = .outside
            step(b)
        }
    }

    private func dispatch() {
        // Decode the byte-buffered body as UTF-8 once, here. Invalid
        // sequences become U+FFFD via the lossy decoder — better than
        // dropping the whole OSC when one stray byte interrupts what's
        // otherwise a valid title.
        let body = String(decoding: bodyBytes, as: UTF8.self)
        switch num {
        case "0", "1", "2":
            if !body.isEmpty {
                pending.append(.title(body))
            }
        case "7":
            if let p = parseOsc7(body) {
                pending.append(.pwd(p))
            }
        case "9":
            if isConEmuBody(body) { return }
            pending.append(.notification(title: body, body: ""))
        case "777":
            let parts = body.split(separator: ";", maxSplits: 2, omittingEmptySubsequences: false)
            if parts.count >= 2 && parts[0] == "notify" {
                let title = String(parts[1])
                let body = parts.count == 3 ? String(parts[2]) : ""
                pending.append(.notification(title: title, body: body))
            }
        case "10", "11", "12":
            if body == "?" {
                let n = UInt8(num) ?? 0
                if n == 10 || n == 11 || n == 12 {
                    pending.append(.colorQuery(n))
                }
            }
        case "22":
            // Set mouse pointer shape (W3C cursor name). Pass through
            // verbatim — the UI maps empty + unknown to default.
            pending.append(.mouseShape(body))
        case "133":
            // Shell-integration prompt/command mark; surface the raw
            // body (A/B/C/D[;exit]). applyOSC maps it to tab state (P4b).
            pending.append(.commandMark(body))
        case "52":
            // OSC 52 program-initiated clipboard write. Body: `Ps;Pc`
            // (selector + base64). Read requests (`Pc == "?"`) drop —
            // phase 1 is write-only. Invalid base64 / non-UTF-8 also
            // drop silently, matching the Rust scanner + Ghostty.
            //
            // Truncated bodies drop entirely: a partial base64 of
            // "hello world" would silently write a shorter wrong
            // string to the user's clipboard. Better to lose the
            // write than corrupt the clipboard.
            if bodyTruncated { return }
            if let event = parseOsc52(body) {
                pending.append(event)
            }
        default:
            break
        }
    }
}

// MARK: - Helpers (port of P4's free fns)

/// True if an OSC 9 body looks like a ConEmu extension rather
/// than an iTerm2 notification.
private func isConEmuBody(_ body: String) -> Bool {
    let bytes = Array(body.utf8)
    guard let first = bytes.first, first.isAsciiDigit else { return false }
    var n = 0
    var i = 0
    while i < bytes.count && bytes[i].isAsciiDigit {
        if n < 100 {
            n = n * 10 + Int(bytes[i] - UInt8(ascii: "0"))
        } else {
            n = 100
        }
        i += 1
    }
    if !(1...12).contains(n) { return false }
    return i == bytes.count || bytes[i] == UInt8(ascii: ";")
}

/// Decode an OSC 7 body of the form `file://[host]/path` into the
/// Decode an OSC 52 body of the form `Ps;Pc` into a `.clipboard`
/// event. Returns nil for read requests (`Pc == "?"`), invalid
/// base64, non-UTF-8 payloads, empty payloads, or unrecognized
/// selectors. Mirrors `crates/roost-osc/src/lib.rs::parse_osc52` 1:1.
///
/// Selector matching is **exact**: empty or `"c"` → system;
/// `"p"` or `"s"` → selection. Multi-character selectors (`"cp"`)
/// and unknown single-character selectors are dropped, matching
/// Ghostty's parser. PR #154 originally coalesced unknown selectors
/// to system; this is the tightened form.
private func parseOsc52(_ body: String) -> OscEvent? {
    guard let semi = body.firstIndex(of: ";") else { return nil }
    let ps = String(body[..<semi])
    let pc = String(body[body.index(after: semi)...])
    if pc == "?" { return nil }
    let target: OscClipboardTarget
    switch ps {
    case "", "c": target = .system
    case "p", "s": target = .selection
    default: return nil
    }
    // Some emitters wrap long base64 over multiple lines; strip
    // whitespace before decoding (`Data(base64Encoded:)` would
    // otherwise reject the wrapped form).
    let cleaned = pc.filter { !$0.isWhitespace }
    guard let data = Data(base64Encoded: cleaned) else { return nil }
    guard let text = String(data: data, encoding: .utf8), !text.isEmpty else {
        // Empty payload is a deliberate divergence from Ghostty
        // (which treats it as "clear the clipboard"). Roost drops
        // it — remote clearing is hostile and no real emitter does
        // it on purpose.
        return nil
    }
    return .clipboard(target: target, text: text)
}

/// percent-decoded path. Returns nil for non-file URIs or
/// malformed percent-encoding.
private func parseOsc7(_ body: String) -> String? {
    guard body.hasPrefix("file://") else { return nil }
    let rest = String(body.dropFirst("file://".count))
    guard let slash = rest.firstIndex(of: "/") else { return nil }
    let path = String(rest[slash...])
    // Foundation's `removingPercentEncoding` returns nil on
    // malformed encoding (matches Go's `url.PathUnescape` failure
    // mode and P4's `percent_decode`'s `None` return).
    return path.removingPercentEncoding
}

private extension UInt8 {
    var isAsciiDigit: Bool { (UInt8(ascii: "0")...UInt8(ascii: "9")).contains(self) }
}
