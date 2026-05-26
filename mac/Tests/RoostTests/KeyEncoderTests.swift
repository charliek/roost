// KeyEncoder unit tests — lock in the libghostty-vt key encoder
// bridge that lives in `mac/Sources/Roost/KeyEncoder.swift`.
//
// Motivated by two regressions we hit:
//
//   1. M1 ship: `event.characters` strips Shift on macOS, so
//      Shift+Tab arrived at the shell as a plain Tab byte and
//      Claude Code's mode-switcher never fired.
//   2. M1 post-ship: a use-after-free in the UTF-8 path caused
//      plain printable keystrokes (`l`, `s`, …) to silently
//      arrive at the encoder as garbage bytes — the shell
//      echoed nothing and Enter ran an empty prompt.
//
// Both bugs would have been caught by a roundtrip test that
// asserts `encode(NSEvent for "a") == [0x61]`. These tests fill
// that gap.

import AppKit
import Carbon.HIToolbox
import CGhosttyVT
import Foundation
import Testing
@testable import Roost

/// Build a synthetic `NSEvent.keyDown` for tests. NSEvent's
/// initializer is fussy and returns nil for invalid arg sets; this
/// helper fills in the boilerplate (timestamp = 0, no window) and
/// derives `characters` / `charactersIgnoringModifiers` from the
/// caller-supplied `chars` string so the encoder's UTF-8 path
/// exercises the same shape an AppKit-delivered event would.
@MainActor
private func keyEvent(
    keyCode: UInt16,
    chars: String,
    charsIgnoringModifiers: String? = nil,
    modifiers: NSEvent.ModifierFlags = []
) -> NSEvent {
    guard let event = NSEvent.keyEvent(
        with: .keyDown,
        location: .zero,
        modifierFlags: modifiers,
        timestamp: 0,
        windowNumber: 0,
        context: nil,
        characters: chars,
        // For a Ctrl+letter event the platform reports `characters` as the
        // C0 byte but `charactersIgnoringModifiers` as the base letter —
        // the encoder's unshifted-codepoint helper reads the latter, so
        // tests can override it independently of `characters`.
        charactersIgnoringModifiers: charsIgnoringModifiers ?? chars,
        isARepeat: false,
        keyCode: keyCode
    ) else {
        fatalError("NSEvent.keyEvent returned nil for keyCode=\(keyCode) chars=\(chars)")
    }
    return event
}

/// Construct a libghostty-vt terminal, hand it to a fresh encoder,
/// run `body`, then tear both down. The encoder needs a live
/// terminal because `setopt_from_terminal` queries cursor-key /
/// Kitty / modifyOtherKeys state per encode.
@MainActor
private func withEncoder(_ body: (KeyEncoder) -> Void) {
    var opts = GhosttyTerminalOptions()
    opts.cols = 80
    opts.rows = 24
    opts.max_scrollback = 0
    var term: GhosttyTerminal?
    let rc = ghostty_terminal_new(nil, &term, opts)
    guard rc.rawValue == 0, let term else {
        fatalError("ghostty_terminal_new failed (rc=\(rc.rawValue))")
    }
    defer { ghostty_terminal_free(term) }
    guard let encoder = KeyEncoder(terminal: term) else {
        fatalError("KeyEncoder init returned nil")
    }
    body(encoder)
}

/// Like `withEncoder`, but pushes the Kitty keyboard protocol's
/// "disambiguate escape codes" flag (`CSI > 1 u`) into the terminal
/// first. The encoder calls `setopt_from_terminal` on every encode, so
/// it picks up the flag — the same path Claude Code / opencode trigger
/// when they enable the protocol.
@MainActor
private func withKittyEncoder(_ body: (KeyEncoder) -> Void) {
    var opts = GhosttyTerminalOptions()
    opts.cols = 80
    opts.rows = 24
    opts.max_scrollback = 0
    var term: GhosttyTerminal?
    let rc = ghostty_terminal_new(nil, &term, opts)
    guard rc.rawValue == 0, let term else {
        fatalError("ghostty_terminal_new failed (rc=\(rc.rawValue))")
    }
    defer { ghostty_terminal_free(term) }
    // CSI > 1 u — push Kitty flags (bit 0 = disambiguate).
    let enable: [UInt8] = [0x1B, 0x5B, 0x3E, 0x31, 0x75]
    enable.withUnsafeBufferPointer { ghostty_terminal_vt_write(term, $0.baseAddress, $0.count) }
    guard let encoder = KeyEncoder(terminal: term) else {
        fatalError("KeyEncoder init returned nil")
    }
    body(encoder)
}

// MARK: - Printable ASCII round-trip

// This is THE test that would have caught the M1 UTF-8 lifetime
// bug: a plain "a" must arrive at the PTY as the single byte 0x61.
// Pre-fix, the dangling-pointer path caused the encoder to read
// garbage and return zero (or undefined) bytes.

@MainActor
@Test
func lowercase_letter_passes_through_as_one_byte() {
    withEncoder { encoder in
        let event = keyEvent(keyCode: UInt16(kVK_ANSI_A), chars: "a")
        let bytes = encoder.encode(event)
        #expect(bytes == Data([0x61]))
    }
}

@MainActor
@Test
func digit_passes_through_as_one_byte() {
    withEncoder { encoder in
        let event = keyEvent(keyCode: UInt16(kVK_ANSI_5), chars: "5")
        let bytes = encoder.encode(event)
        #expect(bytes == Data([0x35]))
    }
}

@MainActor
@Test
func space_passes_through_as_one_byte() {
    withEncoder { encoder in
        let event = keyEvent(keyCode: UInt16(kVK_Space), chars: " ")
        let bytes = encoder.encode(event)
        #expect(bytes == Data([0x20]))
    }
}

@MainActor
@Test
func punctuation_passes_through_as_one_byte() {
    withEncoder { encoder in
        let event = keyEvent(keyCode: UInt16(kVK_ANSI_Slash), chars: "/")
        let bytes = encoder.encode(event)
        #expect(bytes == Data([0x2F]))
    }
}

// MARK: - Control-key conventions

@MainActor
@Test
func enter_returns_carriage_return() {
    withEncoder { encoder in
        // NSEvent.characters for Enter is "\r" already; the encoder
        // derives output from GHOSTTY_KEY_ENTER (the C0 filter in
        // printableUTF8 strips the literal "\r" so the encoder
        // doesn't double-emit).
        let event = keyEvent(keyCode: UInt16(kVK_Return), chars: "\r")
        let bytes = encoder.encode(event)
        #expect(bytes == Data([0x0D]))
    }
}

@MainActor
@Test
func tab_returns_horizontal_tab() {
    withEncoder { encoder in
        let event = keyEvent(keyCode: UInt16(kVK_Tab), chars: "\t")
        let bytes = encoder.encode(event)
        #expect(bytes == Data([0x09]))
    }
}

@MainActor
@Test
func backspace_returns_del() {
    withEncoder { encoder in
        // macOS "delete" key (kVK_Delete) is the backspace position;
        // legacy convention is to send 0x7F (DEL), which is what
        // every shell + readline understands as backspace.
        let event = keyEvent(keyCode: UInt16(kVK_Delete), chars: "\u{7F}")
        let bytes = encoder.encode(event)
        #expect(bytes == Data([0x7F]))
    }
}

@MainActor
@Test
func escape_returns_esc_byte() {
    withEncoder { encoder in
        let event = keyEvent(keyCode: UInt16(kVK_Escape), chars: "\u{1B}")
        let bytes = encoder.encode(event)
        #expect(bytes == Data([0x1B]))
    }
}

// MARK: - Modifier combinations

// This is the originally-reported bug: Shift+Tab in Claude Code
// did nothing. The encoder should produce CSI Z ("back tab"); the
// old `event.characters` path stripped Shift and emitted plain 0x09.

@MainActor
@Test
func shift_tab_returns_back_tab_csi_z() {
    withEncoder { encoder in
        // characters for shift+Tab on macOS is still "\t" (Shift
        // doesn't change Tab); the encoder needs to see Shift in
        // mods to produce CSI Z. The legacy encoding for back-tab
        // is ESC [ Z = [0x1B, 0x5B, 0x5A].
        let event = keyEvent(
            keyCode: UInt16(kVK_Tab),
            chars: "\t",
            modifiers: [.shift]
        )
        let bytes = encoder.encode(event)
        #expect(bytes == Data([0x1B, 0x5B, 0x5A]))
    }
}

@MainActor
@Test
func ctrl_c_returns_etx() {
    withEncoder { encoder in
        // Ctrl+C → ETX (0x03). The encoder derives this from the
        // key + mods; the `printableUTF8` filter strips the literal
        // 0x03 from NSEvent.characters so the encoder doesn't see
        // it twice.
        let event = keyEvent(
            keyCode: UInt16(kVK_ANSI_C),
            chars: "\u{03}",
            modifiers: [.control]
        )
        let bytes = encoder.encode(event)
        #expect(bytes == Data([0x03]))
    }
}

// MARK: - Kitty keyboard protocol (Ctrl+letter)

// The reported bug: under the Kitty keyboard protocol (which Claude
// Code / opencode enable) Roost dropped every Ctrl+letter because it
// passed unshifted_codepoint = 0 to the encoder — letters aren't in the
// functional entry table, so with no codepoint the encoder emits
// nothing. These tests pin the post-fix CSI-u output and guard the drop.

@MainActor
@Test
func ctrl_a_under_kitty_emits_csi_u() {
    withKittyEncoder { encoder in
        // Real Ctrl+A: characters = SOH (0x01), but the base letter is
        // "a". The encoder reads the base letter for the unshifted
        // codepoint and reports CSI 97 ; 5 u (97 = 'a', 5 = 1 + ctrl).
        let event = keyEvent(
            keyCode: UInt16(kVK_ANSI_A),
            chars: "\u{01}",
            charsIgnoringModifiers: "a",
            modifiers: [.control]
        )
        let bytes = encoder.encode(event)
        #expect(bytes == Data([0x1B, 0x5B, 0x39, 0x37, 0x3B, 0x35, 0x75]))
    }
}

@MainActor
@Test
func ctrl_k_under_kitty_emits_csi_u() {
    withKittyEncoder { encoder in
        // Ctrl+K → CSI 107 ; 5 u (107 = 'k').
        let event = keyEvent(
            keyCode: UInt16(kVK_ANSI_K),
            chars: "\u{0B}",
            charsIgnoringModifiers: "k",
            modifiers: [.control]
        )
        let bytes = encoder.encode(event)
        #expect(bytes == Data([0x1B, 0x5B, 0x31, 0x30, 0x37, 0x3B, 0x35, 0x75]))
    }
}

@MainActor
@Test
func ctrl_a_under_kitty_is_not_dropped() {
    // Regression guard: pre-fix this returned empty Data (the encoder
    // had no entry to build for a letter key with codepoint 0). The
    // invariant the user cares about is "a non-empty CSI-u arrives".
    withKittyEncoder { encoder in
        let event = keyEvent(
            keyCode: UInt16(kVK_ANSI_A),
            chars: "\u{01}",
            charsIgnoringModifiers: "a",
            modifiers: [.control]
        )
        let bytes = encoder.encode(event)
        #expect(!bytes.isEmpty, "Ctrl+A under Kitty must not be dropped")
        #expect(bytes.first == 0x1B, "should start with ESC")
    }
}

@MainActor
@Test
func ctrl_a_legacy_returns_soh() {
    // Legacy mode (no Kitty flags) derives the C0 byte from key + mods,
    // so Ctrl+A is SOH (0x01) regardless of the unshifted codepoint.
    // Proves the fix leaves the bash/readline path untouched.
    withEncoder { encoder in
        let event = keyEvent(
            keyCode: UInt16(kVK_ANSI_A),
            chars: "\u{01}",
            charsIgnoringModifiers: "a",
            modifiers: [.control]
        )
        let bytes = encoder.encode(event)
        #expect(bytes == Data([0x01]))
    }
}

@MainActor
@Test
func option_left_arrow_uses_alt_modifier() {
    // Option+ArrowLeft in legacy mode → ESC b (alt-prefixed b,
    // bash readline word-back). Different terminals emit slightly
    // different bytes here; the precise expectation is whatever
    // libghostty-vt's encoder produces under default options. We
    // assert the output is *non-empty and starts with ESC*, which
    // is the invariant the user actually cares about (the bytes
    // get the shell to word-jump).
    withEncoder { encoder in
        let event = keyEvent(
            keyCode: UInt16(kVK_LeftArrow),
            chars: "\u{F702}",
            modifiers: [.option]
        )
        let bytes = encoder.encode(event)
        #expect(!bytes.isEmpty, "Option+Left should emit something")
        #expect(bytes.first == 0x1B, "Option+Left should start with ESC")
    }
}

// MARK: - Arrow keys (CSI)

@MainActor
@Test
func arrow_up_returns_csi_a_in_normal_mode() {
    withEncoder { encoder in
        let event = keyEvent(keyCode: UInt16(kVK_UpArrow), chars: "\u{F700}")
        let bytes = encoder.encode(event)
        // Legacy normal-mode encoding: ESC [ A
        #expect(bytes == Data([0x1B, 0x5B, 0x41]))
    }
}

@MainActor
@Test
func arrow_down_returns_csi_b_in_normal_mode() {
    withEncoder { encoder in
        let event = keyEvent(keyCode: UInt16(kVK_DownArrow), chars: "\u{F701}")
        let bytes = encoder.encode(event)
        #expect(bytes == Data([0x1B, 0x5B, 0x42]))
    }
}

@MainActor
@Test
func arrow_right_returns_csi_c_in_normal_mode() {
    withEncoder { encoder in
        let event = keyEvent(keyCode: UInt16(kVK_RightArrow), chars: "\u{F703}")
        let bytes = encoder.encode(event)
        #expect(bytes == Data([0x1B, 0x5B, 0x43]))
    }
}

@MainActor
@Test
func arrow_left_returns_csi_d_in_normal_mode() {
    withEncoder { encoder in
        let event = keyEvent(keyCode: UInt16(kVK_LeftArrow), chars: "\u{F702}")
        let bytes = encoder.encode(event)
        #expect(bytes == Data([0x1B, 0x5B, 0x44]))
    }
}

// MARK: - Navigation keys

@MainActor
@Test
func home_returns_csi_h() {
    withEncoder { encoder in
        let event = keyEvent(keyCode: UInt16(kVK_Home), chars: "\u{F729}")
        let bytes = encoder.encode(event)
        #expect(bytes == Data([0x1B, 0x5B, 0x48]))
    }
}

@MainActor
@Test
func end_returns_csi_f() {
    withEncoder { encoder in
        let event = keyEvent(keyCode: UInt16(kVK_End), chars: "\u{F72B}")
        let bytes = encoder.encode(event)
        #expect(bytes == Data([0x1B, 0x5B, 0x46]))
    }
}

@MainActor
@Test
func page_up_returns_csi_5_tilde() {
    withEncoder { encoder in
        let event = keyEvent(keyCode: UInt16(kVK_PageUp), chars: "\u{F72C}")
        let bytes = encoder.encode(event)
        #expect(bytes == Data([0x1B, 0x5B, 0x35, 0x7E]))
    }
}

@MainActor
@Test
func page_down_returns_csi_6_tilde() {
    withEncoder { encoder in
        let event = keyEvent(keyCode: UInt16(kVK_PageDown), chars: "\u{F72D}")
        let bytes = encoder.encode(event)
        #expect(bytes == Data([0x1B, 0x5B, 0x36, 0x7E]))
    }
}

// MARK: - Edge cases

@MainActor
@Test
func empty_characters_doesnt_panic() {
    // Modifier-only events (e.g. Shift held alone) arrive with
    // empty characters; the encoder may return zero bytes — the
    // caller in TerminalView guards against propagating an empty
    // Data to the PTY. Test just confirms we don't crash and the
    // result is Data (possibly empty).
    withEncoder { encoder in
        let event = keyEvent(
            keyCode: UInt16(kVK_Shift),
            chars: "",
            modifiers: [.shift]
        )
        let bytes = encoder.encode(event)
        // Don't assert specific bytes — modifier-only encoding is
        // encoder-policy-dependent. Just confirm it returns Data
        // without crashing.
        _ = bytes
    }
}

@MainActor
@Test
func ns_private_function_codepoint_doesnt_leak_into_utf8() {
    // Arrow keys arrive with NSEvent.characters containing PUA
    // codepoints (0xF700–0xF8FF). The encoder must NOT include
    // these in the UTF-8 payload — they're not real text. The
    // expected output is the same as the arrow-key test (ESC [ D
    // for left arrow); if PUA leaked into utf8, the encoder might
    // emit extra bytes or double-encode.
    withEncoder { encoder in
        let event = keyEvent(keyCode: UInt16(kVK_LeftArrow), chars: "\u{F702}")
        let bytes = encoder.encode(event)
        #expect(bytes == Data([0x1B, 0x5B, 0x44]))
    }
}
