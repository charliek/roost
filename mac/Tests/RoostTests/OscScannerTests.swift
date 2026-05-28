// OscScanner unit tests — Swift port of the Rust suite in
// `crates/roost-core/src/osc.rs`. Both implementations are
// byte-equivalent state machines; the Swift port runs in
// `TerminalView.appendBytes` to lift OSC events from the PTY
// stream into the daemon-bound `ReportOsc` RPC.
//
// Test surface mirrors the Rust file 1:1 — when one side gets a
// new case, the other should follow.

import Foundation
import Testing
@testable import Roost

/// Convenience: feed a single byte slice through a fresh scanner
/// and collect every event it produces. Mirrors Rust's `feed_all`.
private func feedAll(_ bytes: [UInt8]) -> [OscEvent] {
    let s = OscScanner()
    return s.feed(Data(bytes))
}

/// Same, accepting a `String` literal payload (treated as UTF-8).
private func feedAll(_ s: String) -> [OscEvent] {
    feedAll(Array(s.utf8))
}

// MARK: - OSC 9 (iTerm2 notification)

@Test
func osc9_bel_terminator() {
    let events = feedAll([0x1B, 0x5D, 0x39, 0x3B, 0x68, 0x65, 0x6C, 0x6C, 0x6F, 0x07])
    #expect(events == [.notification(title: "hello", body: "")])
}

@Test
func osc9_st_terminator() {
    // ESC ] 9 ; hello ESC \
    let bytes: [UInt8] = [0x1B, 0x5D, 0x39, 0x3B] + Array("hello".utf8) + [0x1B, 0x5C]
    #expect(feedAll(bytes) == [.notification(title: "hello", body: "")])
}

@Test
func osc9_split_across_feeds() {
    let s = OscScanner()
    let first = s.feed(Data([0x1B, 0x5D, 0x39, 0x3B] + Array("hel".utf8)))
    #expect(first.isEmpty)
    let second = s.feed(Data(Array("lo".utf8) + [0x07]))
    #expect(second == [.notification(title: "hello", body: "")])
}

@Test
func osc9_conemu_dropped() {
    // Bare "1" is ConEmu sub-command — dropped.
    #expect(feedAll([0x1B, 0x5D, 0x39, 0x3B, 0x31, 0x07]).isEmpty)
    // With trailing `;...` also dropped.
    let bytes: [UInt8] = [0x1B, 0x5D, 0x39, 0x3B] + Array("5;sleeping".utf8) + [0x07]
    #expect(feedAll(bytes).isEmpty)
}

@Test
func osc9_iterm_numeric_outside_conemu_range() {
    // 42 is outside the ConEmu 1..12 sub-command range → treated as
    // iTerm2 notification with numeric title.
    #expect(
        feedAll([0x1B, 0x5D, 0x39, 0x3B, 0x34, 0x32, 0x07])
            == [.notification(title: "42", body: "")]
    )
}

@Test
func osc9_iterm_starts_with_digit_then_text() {
    // "1 file changed" — digit then space → not a ConEmu sub-command.
    let bytes: [UInt8] = [0x1B, 0x5D, 0x39, 0x3B] + Array("1 file changed".utf8) + [0x07]
    #expect(feedAll(bytes) == [.notification(title: "1 file changed", body: "")])
}

// MARK: - OSC 777 (Konsole notify)

@Test
func osc777_with_body() {
    let bytes: [UInt8] = [0x1B, 0x5D] + Array("777;notify;Build done;Tests passed".utf8) + [0x07]
    #expect(
        feedAll(bytes) == [.notification(title: "Build done", body: "Tests passed")]
    )
}

@Test
func osc777_without_body() {
    let bytes: [UInt8] = [0x1B, 0x5D] + Array("777;notify;Just a title".utf8) + [0x07]
    #expect(feedAll(bytes) == [.notification(title: "Just a title", body: "")])
}

@Test
func osc777_non_notify_dropped() {
    // OSC 777 with a non-`notify` opcode — shouldn't emit.
    let bytes: [UInt8] = [0x1B, 0x5D] + Array("777;set-color;1;ff0000".utf8) + [0x07]
    #expect(feedAll(bytes).isEmpty)
}

// MARK: - OSC 7 (cwd)

@Test
func osc7_simple_path() {
    let bytes: [UInt8] = [0x1B, 0x5D] + Array("7;file:///Users/me/work".utf8) + [0x07]
    #expect(feedAll(bytes) == [.pwd("/Users/me/work")])
}

@Test
func osc7_with_host_ignored() {
    let bytes: [UInt8] = [0x1B, 0x5D] + Array("7;file://myhost/Users/me/work".utf8) + [0x07]
    #expect(feedAll(bytes) == [.pwd("/Users/me/work")])
}

@Test
func osc7_percent_decoded() {
    let bytes: [UInt8] = [0x1B, 0x5D] + Array("7;file:///Users/me/spaces%20here".utf8) + [0x07]
    #expect(feedAll(bytes) == [.pwd("/Users/me/spaces here")])
}

@Test
func osc7_malformed_percent_dropped() {
    let bytes: [UInt8] = [0x1B, 0x5D] + Array("7;file:///bad%ZZ".utf8) + [0x07]
    #expect(feedAll(bytes).isEmpty)
}

@Test
func osc7_trailing_percent_dropped() {
    let bytes: [UInt8] = [0x1B, 0x5D] + Array("7;file:///bad%".utf8) + [0x07]
    #expect(feedAll(bytes).isEmpty)
}

@Test
func osc7_non_file_uri_dropped() {
    let bytes: [UInt8] = [0x1B, 0x5D] + Array("7;ssh://elsewhere/path".utf8) + [0x07]
    #expect(feedAll(bytes).isEmpty)
}

// MARK: - OSC 0/1/2 (title)

@Test
func osc0_title() {
    let bytes: [UInt8] = [0x1B, 0x5D] + Array("0;my window title".utf8) + [0x07]
    #expect(feedAll(bytes) == [.title("my window title")])
}

@Test
func osc1_title() {
    let bytes: [UInt8] = [0x1B, 0x5D] + Array("1;icon".utf8) + [0x07]
    #expect(feedAll(bytes) == [.title("icon")])
}

@Test
func osc2_title() {
    let bytes: [UInt8] = [0x1B, 0x5D] + Array("2;window-only title".utf8) + [0x07]
    #expect(feedAll(bytes) == [.title("window-only title")])
}

@Test
func empty_title_dropped() {
    let bytes: [UInt8] = [0x1B, 0x5D, 0x30, 0x3B, 0x07]
    #expect(feedAll(bytes).isEmpty)
}

@Test
func osc_title_preserves_utf8_multibyte() {
    // 🟢 = U+1F7E2 = UTF-8 F0 9F 9F A2. The earlier buggy
    // implementation pushed each byte as a separate Unicode
    // scalar, mangling this into "ð¢" (Latin-1 decoding) in tab
    // titles. After the byte-buffered fix the title round-trips
    // intact. Direct mirror of the Rust test of the same name.
    let title = "🟢 /Users/charliek/projects/roost"
    let bytes: [UInt8] = [0x1B, 0x5D] + Array("0;".utf8) + Array(title.utf8) + [0x07]
    #expect(feedAll(bytes) == [.title(title)])
}

@Test
func osc_title_preserves_cjk() {
    // 日本語 — three CJK ideographs, each 3 UTF-8 bytes. Same
    // round-trip guarantee as the emoji test.
    let title = "日本語"
    let bytes: [UInt8] = [0x1B, 0x5D] + Array("0;".utf8) + Array(title.utf8) + [0x07]
    #expect(feedAll(bytes) == [.title(title)])
}

// MARK: - OSC 10/11/12 (color queries)

@Test
func osc10_query_emits() {
    let bytes: [UInt8] = [0x1B, 0x5D] + Array("10;?".utf8) + [0x07]
    #expect(feedAll(bytes) == [.colorQuery(10)])
}

@Test
func osc11_query_emits() {
    let bytes: [UInt8] = [0x1B, 0x5D] + Array("11;?".utf8) + [0x07]
    #expect(feedAll(bytes) == [.colorQuery(11)])
}

@Test
func osc10_set_dropped() {
    // Set-color body shouldn't emit (libghostty handles).
    let bytes: [UInt8] = [0x1B, 0x5D] + Array("10;rgb:00/00/00".utf8) + [0x07]
    #expect(feedAll(bytes).isEmpty)
}

// MARK: - Multi-sequence + edge cases

@Test
func back_to_back_sequences() {
    // Split into intermediate vars — a single multi-segment `+`
    // chain over [UInt8] + Array(_.utf8) trips the Swift type
    // checker on CI ("unable to type-check this expression in
    // reasonable time").
    var bytes: [UInt8] = []
    bytes += [0x1B, 0x5D]; bytes += Array("0;t1".utf8); bytes += [0x07]
    bytes += [0x1B, 0x5D]; bytes += Array("7;file:///a".utf8); bytes += [0x07]
    bytes += [0x1B, 0x5D]; bytes += Array("9;notif".utf8); bytes += [0x07]
    let events = feedAll(bytes)
    #expect(events == [
        .title("t1"),
        .pwd("/a"),
        .notification(title: "notif", body: ""),
    ])
}

@Test
func malformed_st_recovers_following_osc() {
    // ESC followed by non-`\` aborts the in-flight sequence, but
    // the byte is re-fed so a fresh OSC starting with ESC isn't
    // lost. Mirrors the Rust + Go scanner contract.
    var bytes: [UInt8] = []
    bytes += [0x1B, 0x5D]; bytes += Array("9;abc".utf8); bytes += [0x1B, 0x58]  // ESC then X (bogus)
    bytes += [0x1B, 0x5D]; bytes += Array("7;file:///b".utf8); bytes += [0x07]
    #expect(feedAll(bytes) == [.pwd("/b")])
}

@Test
func body_truncates_at_max() {
    // 10KB body should truncate at maxBody (8192 bytes). The Swift
    // scanner's maxBody is file-private, so we assert against the
    // documented value (8192).
    let prefix: [UInt8] = [0x1B, 0x5D, 0x30, 0x3B]  // ESC ] 0 ;
    let payload = prefix + Array(repeating: UInt8(ascii: "A"), count: 10_000) + [0x07]
    let events = feedAll(payload)
    #expect(events.count == 1)
    if case .title(let t) = events[0] {
        #expect(t.utf8.count == 8192)
    } else {
        Issue.record("expected .title event")
    }
}

@Test
func unrelated_bytes_pass_through() {
    // Non-OSC bytes should leave the scanner at Outside and emit
    // nothing.
    let bytes: [UInt8] = Array("some shell output\nmore output\n".utf8)
    #expect(feedAll(bytes).isEmpty)
}

// MARK: - OSC 133 (shell-integration prompt/command marks)

@Test
func osc133_command_start() {
    #expect(feedAll("\u{1b}]133;C\u{07}") == [.commandMark("C")])
}

@Test
func osc133_command_end_with_exit_st_terminator() {
    // ESC ] 133 ; D ; 0 ESC \  — the exit code stays in the body.
    #expect(feedAll("\u{1b}]133;D;0\u{1b}\\") == [.commandMark("D;0")])
}

@Test
func osc133_split_across_feeds() {
    let s = OscScanner()
    #expect(s.feed(Data(Array("\u{1b}]133;".utf8))).isEmpty)
    #expect(s.feed(Data(Array("A\u{07}".utf8))) == [.commandMark("A")])
}

@Test
func osc133_interleaved_with_pwd() {
    #expect(
        feedAll("\u{1b}]133;C\u{07}\u{1b}]7;file:///tmp\u{07}")
            == [.commandMark("C"), .pwd("/tmp")]
    )
}

@Test
func osc133_bare_no_body() {
    // Malformed (no kind letter) -> empty mark; harmless downstream.
    #expect(feedAll("\u{1b}]133\u{07}") == [.commandMark("")])
}

@Test
func osc133_empty_body() {
    #expect(feedAll("\u{1b}]133;\u{07}") == [.commandMark("")])
}

// MARK: - OSC 52 (program-initiated clipboard write)

/// Convenience: base64-encode a UTF-8 string for OSC 52 payloads.
private func b64(_ s: String) -> String {
    Data(s.utf8).base64EncodedString()
}

@Test
func osc52_c_target_decodes_payload() {
    let payload = "\u{1b}]52;c;\(b64("hello-osc52"))\u{07}"
    #expect(feedAll(payload) == [.clipboard(target: .system, text: "hello-osc52")])
}

@Test
func osc52_p_target_routes_to_selection() {
    let payload = "\u{1b}]52;p;\(b64("primary text"))\u{07}"
    #expect(feedAll(payload) == [.clipboard(target: .selection, text: "primary text")])
}

@Test
func osc52_empty_selector_defaults_to_system() {
    // OSC 52 ; ; <base64> — some emitters omit the selector.
    let payload = "\u{1b}]52;;\(b64("defaulted"))\u{07}"
    #expect(feedAll(payload) == [.clipboard(target: .system, text: "defaulted")])
}

@Test
func osc52_read_request_dropped() {
    // Pc == "?" — read request, dropped in phase 1.
    #expect(feedAll("\u{1b}]52;c;?\u{07}") == [])
}

@Test
func osc52_invalid_base64_dropped() {
    #expect(feedAll("\u{1b}]52;c;!!!not-base64!!!\u{07}") == [])
}

@Test
func osc52_non_utf8_payload_dropped() {
    // Valid base64 of three invalid-UTF-8 bytes (0xFF 0xFE 0xFD).
    let badB64 = Data([0xFF, 0xFE, 0xFD]).base64EncodedString()
    #expect(feedAll("\u{1b}]52;c;\(badB64)\u{07}") == [])
}

@Test
func osc52_empty_payload_dropped() {
    #expect(feedAll("\u{1b}]52;c;\u{07}") == [])
}

@Test
func osc52_multi_char_selector_dropped() {
    // OSC 52's selector is at most one character; `cp` is malformed
    // per the spec. PR #154 originally coalesced this to system; the
    // fixup PR tightened to drop, matching Ghostty's exact-match parser.
    let payload = "\u{1b}]52;cp;\(b64("ignored"))\u{07}"
    #expect(feedAll(payload) == [])
}

@Test
func osc52_lone_unknown_selector_dropped() {
    // Single-char unknown selectors (e.g. `q`) also drop — no `q`
    // selector in the spec, and silently coalescing to system masks
    // emitter bugs.
    let payload = "\u{1b}]52;q;\(b64("ignored"))\u{07}"
    #expect(feedAll(payload) == [])
}

@Test
func osc52_truncated_body_drops_event() {
    // A truncated OSC 52 body must NOT emit — partial base64 would
    // silently write the wrong text to the clipboard. Pump enough
    // bytes past `maxBody` to flip the truncation flag.
    var payload: [UInt8] = [0x1B, 0x5D, 0x35, 0x32, 0x3B, 0x63, 0x3B]
    payload.append(contentsOf: Array(repeating: UInt8(ascii: "A"), count: 1024 * 1024 + 512))
    payload.append(0x07)
    #expect(feedAll(payload) == [])
}

@Test
func osc52_st_terminator_works() {
    let bytes: [UInt8] =
        [0x1B, 0x5D, 0x35, 0x32, 0x3B, 0x63, 0x3B]
        + Array(b64("st-terminated").utf8)
        + [0x1B, 0x5C]
    #expect(feedAll(bytes) == [.clipboard(target: .system, text: "st-terminated")])
}
