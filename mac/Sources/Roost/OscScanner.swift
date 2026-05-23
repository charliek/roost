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

private let maxBody = 8192

/// One parsed-out OSC event. Returned in order by
/// `OscScanner.feed`.
enum OscEvent: Equatable {
    case title(String)
    case pwd(String)
    case notification(title: String, body: String)
    case colorQuery(UInt8)  // 10, 11, or 12

    /// Maps a parsed event to the (osc_command, payload) pair the
    /// `ReportOsc` RPC expects. Used by `TerminalView.appendBytes`
    /// to bridge scanner output into the existing gRPC surface.
    var asReport: (UInt32, String) {
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
