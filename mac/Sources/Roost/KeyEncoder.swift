// libghostty-vt key encoder bridge for the Mac UI.
//
// Phase 6a M1 follow-up (goal-mac-polish-cursor-keys-2026-05-17).
//
// Replaces TerminalView.keyDown's hand-rolled byte path (`specialKeyBytes`
// table + raw `NSEvent.characters` fall-through) with a real call into
// libghostty-vt's encoder. Fixes Shift+Tab, Shift+Enter, Option+Arrow,
// Ctrl+letter, and any other modifier+key combination — `NSEvent.characters`
// strips Shift before yielding bytes, so the old path was lossy by design.
//
// One `KeyEncoder` per `TerminalView`. Encoder + reusable event are
// allocated in `init` and freed in `deinit`. Before each encode we call
// `ghostty_key_encoder_setopt_from_terminal` so cursor-key-application
// mode, Kitty keyboard flags, modifyOtherKeys state, etc. follow the
// live terminal — Claude Code's mode toggles, for instance, take effect
// on the very next keystroke. The same invariant holds: the encoder is
// re-synced from libghostty-vt's terminal state on every encode.
//
// The keycode → `GhosttyKey` table mirrors W3C UI Events keyboard codes,
// which is what `GhosttyKey` enumerates. We translate Carbon `kVK_*`
// constants (what NSEvent.keyCode reports) → the matching enum value.
// Unknown keycodes map to `GHOSTTY_KEY_UNIDENTIFIED` — the encoder will
// then fall back on the UTF-8 text payload, which still works for plain
// printable characters under unusual layouts.

import AppKit
import Carbon.HIToolbox
import CGhosttyVT
import Foundation

@MainActor
final class KeyEncoder {
    /// `nonisolated(unsafe)` matches TerminalView's terminal-handle
    /// pattern: opaque libghostty-vt pointers aren't Sendable, and
    /// Swift 6 strict concurrency otherwise forbids the `@MainActor`-
    /// implicit property from being read in `deinit` (which is itself
    /// nonisolated). Safe here because the handles are allocated on
    /// the main thread in `init`, only ever read from main-thread
    /// `encode()`, and freed on the main thread when the KeyEncoder
    /// is torn down alongside its TerminalView.
    nonisolated(unsafe) private let encoder: GhosttyKeyEncoder
    nonisolated(unsafe) private let event: GhosttyKeyEvent
    nonisolated(unsafe) private let terminal: GhosttyTerminal

    /// Initialize a new encoder bound to a live terminal handle. The
    /// terminal must outlive this encoder — `TerminalView` enforces
    /// that by owning both and tearing them down in lockstep in its
    /// deinit (encoder freed implicitly via this class's deinit, then
    /// terminal freed by the view).
    init?(terminal: GhosttyTerminal) {
        var enc: GhosttyKeyEncoder?
        let rcEncoder = ghostty_key_encoder_new(nil, &enc)
        guard rcEncoder.rawValue == GHOSTTY_SUCCESS.rawValue, let enc else {
            return nil
        }
        var ev: GhosttyKeyEvent?
        let rcEvent = ghostty_key_event_new(nil, &ev)
        guard rcEvent.rawValue == GHOSTTY_SUCCESS.rawValue, let ev else {
            ghostty_key_encoder_free(enc)
            return nil
        }
        self.encoder = enc
        self.event = ev
        self.terminal = terminal
    }

    deinit {
        ghostty_key_event_free(event)
        ghostty_key_encoder_free(encoder)
    }

    /// Encode a synthetic key event (no NSEvent, no UTF-8 text).
    /// Used by M6's alt-screen wheel translation: each scroll-up
    /// row dispatches one `ARROW_UP` press through the same encoder
    /// path real keystrokes use, so DECCKM application-mode + Kitty
    /// flags are honored without hand-rolling escape sequences.
    func encode(syntheticKey key: GhosttyKey, mods: GhosttyMods = 0) -> Data {
        ghostty_key_encoder_setopt_from_terminal(encoder, terminal)
        ghostty_key_event_set_action(event, GHOSTTY_KEY_ACTION_PRESS)
        ghostty_key_event_set_key(event, key)
        ghostty_key_event_set_mods(event, mods)
        ghostty_key_event_set_consumed_mods(event, 0)
        ghostty_key_event_set_unshifted_codepoint(event, 0)
        ghostty_key_event_set_composing(event, false)
        ghostty_key_event_set_utf8(event, nil, 0)

        var stackBuf = [CChar](repeating: 0, count: 128)
        var written: size_t = 0
        let rc = stackBuf.withUnsafeMutableBufferPointer { buf in
            ghostty_key_encoder_encode(encoder, event, buf.baseAddress, buf.count, &written)
        }
        if rc.rawValue == GHOSTTY_SUCCESS.rawValue {
            return Self.dataFromCChars(stackBuf, count: written)
        }
        guard written > 0 else { return Data() }
        var dynBuf = [CChar](repeating: 0, count: written)
        let rc2 = dynBuf.withUnsafeMutableBufferPointer { buf in
            ghostty_key_encoder_encode(encoder, event, buf.baseAddress, buf.count, &written)
        }
        if rc2.rawValue == GHOSTTY_SUCCESS.rawValue {
            return Self.dataFromCChars(dynBuf, count: written)
        }
        return Data()
    }

    /// Encode an NSEvent into bytes for the PTY. Empty Data means the
    /// encoder swallowed the event (modifier-only presses, IME-driven
    /// dead keys, etc.); the caller should NOT write zero bytes.
    func encode(_ nsEvent: NSEvent) -> Data {
        // Sync encoder options from the live terminal *before* every
        // encode. Terminal modes (DECCKM, DECPAM, Kitty flags, etc.)
        // can flip between keystrokes — typical case is an app toggling
        // cursor-key-application mode on focus.
        ghostty_key_encoder_setopt_from_terminal(encoder, terminal)

        let key = Self.ghosttyKey(forKeyCode: nsEvent.keyCode)
        let mods = Self.ghosttyMods(forFlags: nsEvent.modifierFlags)
        let chord = Self.controlChord(for: nsEvent, key: key)
        let action: GhosttyKeyAction = nsEvent.isARepeat
            ? GHOSTTY_KEY_ACTION_REPEAT
            : GHOSTTY_KEY_ACTION_PRESS

        ghostty_key_event_set_action(event, action)
        ghostty_key_event_set_key(event, key)
        ghostty_key_event_set_mods(event, mods)
        // Modifiers used to produce printable text are not effective
        // terminal-key modifiers. Without this, Kitty disambiguation turns
        // Shift+G into CSI 103;2u ("g" + Shift) instead of the text "G";
        // crossterm then exposes the base "g" to apps such as strix. AppKit
        // does not expose exact consumed modifiers, so match Ghostty's
        // long-standing heuristic: Shift and Option participate in text
        // translation, while Control and Command do not.
        //
        // libghostty consults consumed_mods ONLY when utf8 is non-empty
        // (`KeyEvent.effectiveMods`), and `printableUTF8` below strips C0 —
        // so Enter/Tab/Escape/Backspace arrive with empty utf8 and keep their
        // full modifier set. That's what leaves Shift+Enter encoding as
        // CSI 13;2u. If that C0 filter ever goes away, this consumed-mods
        // value starts applying to those keys too.
        //
        // A recovered chord reports nothing consumed: its utf8 is our own
        // synthesis (the platform handed us a C0), so no modifier went into
        // producing it, and subtracting Option would clear the effective alt
        // that puts the ESC meta-prefix on ctrl+option chords.
        ghostty_key_event_set_consumed_mods(
            event,
            chord == nil ? Self.consumedMods(forFlags: nsEvent.modifierFlags) : 0
        )
        // The unshifted base-layout codepoint (Ctrl+A → "a" = 97). Under
        // the Kitty keyboard protocol the encoder needs this to build a
        // CSI-u entry for letter/digit keys — they're not in the
        // functional `kitty_entries` table, so with 0 the encoder emits
        // NOTHING for Ctrl+letter (Claude Code / opencode enable Kitty,
        // so every Ctrl+letter was silently dropped). Legacy mode derives
        // the C0 byte from the key alone and is unaffected either way.
        //
        // A recovered chord names its own key: see `ControlChord.key`.
        ghostty_key_event_set_unshifted_codepoint(
            event,
            chord.flatMap(\.key)?.value ?? Self.unshiftedCodepoint(for: nsEvent)
        )
        // IME composition is handled by NSResponder via interpretKeyEvents
        // → insertText (the path TerminalView doesn't currently wire);
        // when this method runs, we're past composition.
        ghostty_key_event_set_composing(event, false)

        // UTF-8 text. The C API requires us to strip C0 control chars
        // (0x00-0x1F and 0x7F) — the encoder re-derives them from
        // key+mods. Without this filter, Ctrl+A would arrive as both
        // a key+mods encoding AND a literal 0x01 byte in `utf8`.
        //
        // `ghostty_key_event_set_utf8` does NOT take ownership of the
        // pointer (per the header docs); the encode call below reads
        // it. So we MUST keep the bytes alive until after the encode
        // returns — both calls live inside the same `withUnsafeBytes`
        // scope. Setting the pointer and letting it dangle before the
        // encode call is the bug we shipped in M1: ASCII characters
        // arrived at the encoder as garbage, so the shell never
        // received "l" or "s" but the empty-line Enter still made it
        // through (Enter's "\r" gets filtered to empty UTF-8 and the
        // encoder derives from key alone, so it wasn't affected).
        let utf8 = chord?.text ?? Self.printableUTF8(for: nsEvent)
        return utf8.withUnsafeBytes { (raw: UnsafeRawBufferPointer) -> Data in
            if let base = raw.bindMemory(to: CChar.self).baseAddress, raw.count > 0 {
                ghostty_key_event_set_utf8(event, base, raw.count)
            } else {
                ghostty_key_event_set_utf8(event, nil, 0)
            }

            // Try a 128-byte stack buffer first — covers every legacy
            // and most Kitty-protocol encodings (the longest forms top
            // out around ~40 bytes). Grow on OUT_OF_SPACE.
            var stackBuf = [CChar](repeating: 0, count: 128)
            var written: size_t = 0
            let rc = stackBuf.withUnsafeMutableBufferPointer { buf in
                ghostty_key_encoder_encode(encoder, event, buf.baseAddress, buf.count, &written)
            }
            if rc.rawValue == GHOSTTY_SUCCESS.rawValue {
                return Self.dataFromCChars(stackBuf, count: written)
            }
            // OUT_OF_SPACE → `written` contains the required size. Retry
            // with a heap buffer. (The C API contract: query-size returns
            // OUT_OF_SPACE with `written` set; pass NULL+0 to query, but
            // a small stack buffer is faster in the common case.)
            guard written > 0 else { return Data() }
            var dynBuf = [CChar](repeating: 0, count: written)
            let rc2 = dynBuf.withUnsafeMutableBufferPointer { buf in
                ghostty_key_encoder_encode(encoder, event, buf.baseAddress, buf.count, &written)
            }
            if rc2.rawValue == GHOSTTY_SUCCESS.rawValue {
                return Self.dataFromCChars(dynBuf, count: written)
            }
            return Data()
        }
    }

    private static func dataFromCChars(_ buf: [CChar], count: size_t) -> Data {
        guard count > 0 else { return Data() }
        return buf.prefix(count).withUnsafeBufferPointer { ptr in
            Data(bytes: ptr.baseAddress!, count: count)
        }
    }

    /// Build the UTF-8 payload the encoder expects: printable platform
    /// text only. C0 control chars and DEL are stripped per the C API
    /// contract. Returns `Data` (possibly empty) — never returns nil.
    private static func printableUTF8(for nsEvent: NSEvent) -> Data {
        guard let chars = nsEvent.characters, !chars.isEmpty else { return Data() }
        let filtered = chars.unicodeScalars.filter { scalar in
            let value = scalar.value
            // Strip C0 (0x00-0x1F) and DEL (0x7F). Also strip the
            // NS-private function-key codepoints (NSEvent.characters
            // returns 0xF700+ for arrows/Home/PageUp/etc.) — those
            // are key events, not text.
            if value < 0x20 || value == 0x7F { return false }
            if value >= 0xF700 && value <= 0xF8FF { return false }
            return true
        }
        if filtered.isEmpty { return Data() }
        return Data(String(String.UnicodeScalarView(filtered)).utf8)
    }

    /// A control-transformed press, split into the two characters it stands
    /// for: the text it would have typed and the key that typed it.
    private struct ControlChord {
        /// What libghostty receives as the press's utf8 — shift included,
        /// since the control transform's own table is keyed on the character
        /// the layout typed (ctrl+shift+- has to arrive as `_` to fold into
        /// 0x1F).
        let text: Data
        /// The unshifted Latin character that identifies the key, or nil when
        /// the C0 folds in more than one key and only the layout can say.
        ///
        /// AppKit reports the *active layout's* character for a press, which
        /// on a non-Latin layout is not the key libghostty must name: ctrl on
        /// a Russian I arrives as TAB while the layout says that key types
        /// `ш`, and reporting U+0448 puts the Kitty entry on 1096 and drops
        /// the shift bit off the legacy one. A recovered chord's C0 is
        /// layout-blind and inverts to exactly one key for the letters and
        /// `[ \ ]`, so here it names the key — the same identity iced derives
        /// from the physical key (`crates/roost-iced/src/input.rs`, via
        /// `Key::to_latin`).
        let key: Unicode.Scalar?
    }

    /// The printable character behind a control-transformed press and the key
    /// it came from, or nil when this press is not a chord whose C0 byte
    /// inverts unambiguously.
    ///
    /// macOS applies the control transform itself, so `NSEvent.characters`
    /// for ctrl+[ is ESC, for ctrl+i is TAB and for ctrl+m is CR. libghostty
    /// keys its control-sequence table on the *printable* byte and leaves
    /// `[`, `i` and `m` out of it on purpose (fixterms: applications must be
    /// able to tell ctrl+[ from Escape, ctrl+i from Tab, ctrl+m from Enter),
    /// so those three fall through to CSI-u — which needs one codepoint of
    /// utf8 to build an entry from. Stripping the C0 left it with nothing and
    /// all three chords reached the PTY as no bytes at all (#343). Forwarding
    /// the printable instead is what the iced UI does, so both UIs now emit
    /// CSI 91/105/109 ; 5 u (`crates/roost-iced/src/input.rs::control_chord`).
    ///
    /// Only writing-system keys qualify. ctrl+Return, ctrl+Escape and
    /// ctrl+Tab present a lone C0 of their own — the very bytes ctrl+m,
    /// ctrl+[ and ctrl+i fold into — and inverting those would retype three
    /// of the most common keys on the board as letters.
    ///
    /// A Command chord never recovers. macOS treats it as a menu equivalent,
    /// and libghostty's legacy CSI-u fallback has no Super bit to report
    /// (`key_encode.zig`'s `CsiUMods`), so recovering ctrl+cmd+i would reach
    /// the PTY as `ESC [ 105 ; 5 u` — a plain ctrl+i with Command erased.
    /// With no utf8 the press encodes to nothing, which is what it did before
    /// recovery existed.
    private static func controlChord(for e: NSEvent, key: GhosttyKey) -> ControlChord? {
        guard e.modifierFlags.contains(.control),
              !e.modifierFlags.contains(.command),
              Self.isWritingSystemKey(key),
              let c0 = Self.soleScalar(e.characters),
              let canonical = Self.controlInverse(c0)
        else { return nil }
        // `charactersIgnoringModifiers` is the press with every modifier but
        // control applied, and the table is keyed on the character the layout
        // actually typed — ctrl+shift+- has to arrive as `_` to fold into
        // 0x1F. The canonical inverse is the layout-blind fallback that still
        // recovers a non-Latin layout, where the typed character can't
        // confirm the C0 (ctrl+ф → `a`).
        let typed = Self.soleScalar(e.charactersIgnoringModifiers)
        let text = typed.flatMap { Self.controlTransform($0) == c0 ? $0 : nil } ?? canonical
        return ControlChord(
            text: Data(String(Character(text)).utf8),
            // macOS folds both ctrl+- and ctrl+/ onto 0x1F, so that one C0
            // names no single key and the layout stays in charge of it.
            key: canonical == "_" ? nil : canonical
        )
    }

    /// libghostty enumerates W3C UI Events codes and groups "Writing System
    /// Keys" (§ 3.1.1) contiguously, from BACKQUOTE through SLASH. That block
    /// is the Mac spelling of the `Key::Character(_)` gate iced puts on chord
    /// recovery: everything after it is a functional, control-pad, arrow,
    /// numpad or media key, which owns its own encoding.
    private static func isWritingSystemKey(_ key: GhosttyKey) -> Bool {
        key.rawValue >= GHOSTTY_KEY_BACKQUOTE.rawValue
            && key.rawValue <= GHOSTTY_KEY_SLASH.rawValue
    }

    /// The printable character a control transform folded into `c0`, for the
    /// chords that invert unambiguously: the letters and `[ \ ] _`. NUL
    /// (ctrl+space, ctrl+shift+2), RS (ctrl+shift+6) and DEL keep today's
    /// encoding — more than one key folds onto each of those.
    private static func controlInverse(_ c0: Unicode.Scalar) -> Unicode.Scalar? {
        guard let byte = UInt8(exactly: c0.value) else { return nil }
        switch byte {
        case 0x01...0x1A: return Unicode.Scalar(UInt8(ascii: "a") + byte - 1)
        case 0x1B: return Unicode.Scalar(UInt8(ascii: "["))
        case 0x1C: return Unicode.Scalar(UInt8(ascii: "\\"))
        case 0x1D: return Unicode.Scalar(UInt8(ascii: "]"))
        case 0x1F: return Unicode.Scalar(UInt8(ascii: "_"))
        default: return nil
        }
    }

    /// The C0 byte the platform produces for control plus this character,
    /// for the chords `controlInverse` can invert.
    private static func controlTransform(_ value: Unicode.Scalar) -> Unicode.Scalar? {
        guard let byte = UInt8(exactly: value.value) else { return nil }
        switch byte {
        case UInt8(ascii: "a")...UInt8(ascii: "z"):
            return Unicode.Scalar(byte - UInt8(ascii: "a") + 1)
        case UInt8(ascii: "A")...UInt8(ascii: "Z"):
            return Unicode.Scalar(byte - UInt8(ascii: "A") + 1)
        case UInt8(ascii: "["), UInt8(ascii: "{"): return Unicode.Scalar(0x1B as UInt8)
        case UInt8(ascii: "\\"), UInt8(ascii: "|"): return Unicode.Scalar(0x1C as UInt8)
        case UInt8(ascii: "]"), UInt8(ascii: "}"): return Unicode.Scalar(0x1D as UInt8)
        case UInt8(ascii: "_"): return Unicode.Scalar(0x1F as UInt8)
        default: return nil
        }
    }

    private static func soleScalar(_ value: String?) -> Unicode.Scalar? {
        guard let scalars = value?.unicodeScalars, scalars.count == 1 else { return nil }
        return scalars.first
    }

    /// The unshifted base-layout codepoint for `e` — the character the
    /// key would type with no modifiers held (Ctrl+A → "a" = 97;
    /// Ctrl+Shift+K → "k" = 107). The libghostty-vt encoder uses this to
    /// build Kitty CSI-u entries for letter/digit keys. Returns 0 (the
    /// "absent" sentinel the C API expects) for C0/DEL and the NS-private
    /// function-key range, which aren't text. Mirrors cmux's
    /// `unshiftedCodepointFromEvent`.
    ///
    /// `characters(byApplyingModifiers: [])` asks AppKit for the
    /// no-modifier characters, which is the unshifted layout char. For
    /// non-US layouts cmux additionally consults the TIS layout via
    /// `KeyboardLayout.character(forKeyCode:)`; that's a possible
    /// follow-up for exotic layouts but isn't needed for the reported bug.
    private static func unshiftedCodepoint(for e: NSEvent) -> UInt32 {
        let s = e.characters(byApplyingModifiers: []) ?? e.charactersIgnoringModifiers
        guard let scalar = s?.unicodeScalars.first else { return 0 }
        let v = scalar.value
        if v < 0x20 || v == 0x7F { return 0 }        // C0 / DEL
        if v >= 0xF700 && v <= 0xF8FF { return 0 }   // NS private-use fn keys
        return v
    }

    /// Translate NSEvent.modifierFlags to libghostty-vt's mods bitmask.
    /// Side-specific bits (left vs. right shift, etc.) are left unset
    /// — NSEvent doesn't easily expose that without `kCGEventFlags*`
    /// inspection, and most encodings don't depend on it.
    private static func ghosttyMods(forFlags flags: NSEvent.ModifierFlags) -> GhosttyMods {
        var mods: UInt16 = 0
        if flags.contains(.shift)    { mods |= 1 << 0 } // GHOSTTY_MODS_SHIFT
        if flags.contains(.control)  { mods |= 1 << 1 } // GHOSTTY_MODS_CTRL
        if flags.contains(.option)   { mods |= 1 << 2 } // GHOSTTY_MODS_ALT
        if flags.contains(.command)  { mods |= 1 << 3 } // GHOSTTY_MODS_SUPER
        if flags.contains(.capsLock) { mods |= 1 << 4 } // GHOSTTY_MODS_CAPS_LOCK
        return GhosttyMods(mods)
    }

    /// AppKit does not report which modifiers were consumed while translating
    /// a key into text, so Ghostty applies a heuristic: control and command
    /// never contribute to translation, assume everything else did. This is
    /// that heuristic verbatim — Ghostty's `NSEvent.ghosttyKeyEvent` computes
    /// `ghosttyMods((translationMods ?? modifierFlags).subtracting([.control,
    /// .command]))`, and its `translationMods` only diverges from
    /// `modifierFlags` under a non-default `macos-option-as-alt`, which
    /// libghostty-vt pins to `.false` on every
    /// `ghostty_key_encoder_setopt_from_terminal` call.
    ///
    /// Subtracting from the live flags rather than listing bits keeps this
    /// aligned with `ghosttyMods` — any modifier that translator learns to
    /// report is automatically classified the same way Ghostty classifies it.
    private static func consumedMods(forFlags flags: NSEvent.ModifierFlags) -> GhosttyMods {
        ghosttyMods(forFlags: flags.subtracting([.control, .command]))
    }

    /// Translate NSEvent.keyCode (a Carbon kVK_* value) to the
    /// `GhosttyKey` enum (W3C UI Events keyboard codes). Coverage is
    /// "every key that could plausibly land in TerminalView.keyDown
    /// on a Mac keyboard" — letters, digits, punctuation, navigation,
    /// arrows, function keys, numpad. Unrecognized keys fall back on
    /// the UTF-8 text payload, which the encoder handles via the
    /// legacy-text path.
    private static func ghosttyKey(forKeyCode keyCode: UInt16) -> GhosttyKey {
        switch Int(keyCode) {
        // Letters (Writing System Keys)
        case kVK_ANSI_A: return GHOSTTY_KEY_A
        case kVK_ANSI_B: return GHOSTTY_KEY_B
        case kVK_ANSI_C: return GHOSTTY_KEY_C
        case kVK_ANSI_D: return GHOSTTY_KEY_D
        case kVK_ANSI_E: return GHOSTTY_KEY_E
        case kVK_ANSI_F: return GHOSTTY_KEY_F
        case kVK_ANSI_G: return GHOSTTY_KEY_G
        case kVK_ANSI_H: return GHOSTTY_KEY_H
        case kVK_ANSI_I: return GHOSTTY_KEY_I
        case kVK_ANSI_J: return GHOSTTY_KEY_J
        case kVK_ANSI_K: return GHOSTTY_KEY_K
        case kVK_ANSI_L: return GHOSTTY_KEY_L
        case kVK_ANSI_M: return GHOSTTY_KEY_M
        case kVK_ANSI_N: return GHOSTTY_KEY_N
        case kVK_ANSI_O: return GHOSTTY_KEY_O
        case kVK_ANSI_P: return GHOSTTY_KEY_P
        case kVK_ANSI_Q: return GHOSTTY_KEY_Q
        case kVK_ANSI_R: return GHOSTTY_KEY_R
        case kVK_ANSI_S: return GHOSTTY_KEY_S
        case kVK_ANSI_T: return GHOSTTY_KEY_T
        case kVK_ANSI_U: return GHOSTTY_KEY_U
        case kVK_ANSI_V: return GHOSTTY_KEY_V
        case kVK_ANSI_W: return GHOSTTY_KEY_W
        case kVK_ANSI_X: return GHOSTTY_KEY_X
        case kVK_ANSI_Y: return GHOSTTY_KEY_Y
        case kVK_ANSI_Z: return GHOSTTY_KEY_Z

        // Digits (top row)
        case kVK_ANSI_0: return GHOSTTY_KEY_DIGIT_0
        case kVK_ANSI_1: return GHOSTTY_KEY_DIGIT_1
        case kVK_ANSI_2: return GHOSTTY_KEY_DIGIT_2
        case kVK_ANSI_3: return GHOSTTY_KEY_DIGIT_3
        case kVK_ANSI_4: return GHOSTTY_KEY_DIGIT_4
        case kVK_ANSI_5: return GHOSTTY_KEY_DIGIT_5
        case kVK_ANSI_6: return GHOSTTY_KEY_DIGIT_6
        case kVK_ANSI_7: return GHOSTTY_KEY_DIGIT_7
        case kVK_ANSI_8: return GHOSTTY_KEY_DIGIT_8
        case kVK_ANSI_9: return GHOSTTY_KEY_DIGIT_9

        // Punctuation (ANSI)
        case kVK_ANSI_Minus:        return GHOSTTY_KEY_MINUS
        case kVK_ANSI_Equal:        return GHOSTTY_KEY_EQUAL
        case kVK_ANSI_LeftBracket:  return GHOSTTY_KEY_BRACKET_LEFT
        case kVK_ANSI_RightBracket: return GHOSTTY_KEY_BRACKET_RIGHT
        case kVK_ANSI_Backslash:    return GHOSTTY_KEY_BACKSLASH
        case kVK_ANSI_Semicolon:    return GHOSTTY_KEY_SEMICOLON
        case kVK_ANSI_Quote:        return GHOSTTY_KEY_QUOTE
        case kVK_ANSI_Comma:        return GHOSTTY_KEY_COMMA
        case kVK_ANSI_Period:       return GHOSTTY_KEY_PERIOD
        case kVK_ANSI_Slash:        return GHOSTTY_KEY_SLASH
        case kVK_ANSI_Grave:        return GHOSTTY_KEY_BACKQUOTE

        // Functional Keys
        case kVK_Return:       return GHOSTTY_KEY_ENTER
        case kVK_Tab:          return GHOSTTY_KEY_TAB
        case kVK_Space:        return GHOSTTY_KEY_SPACE
        case kVK_Delete:       return GHOSTTY_KEY_BACKSPACE   // Mac "delete" key
        case kVK_Escape:       return GHOSTTY_KEY_ESCAPE
        case kVK_CapsLock:     return GHOSTTY_KEY_CAPS_LOCK
        case kVK_Command:      return GHOSTTY_KEY_META_LEFT
        case kVK_RightCommand: return GHOSTTY_KEY_META_RIGHT
        case kVK_Shift:        return GHOSTTY_KEY_SHIFT_LEFT
        case kVK_RightShift:   return GHOSTTY_KEY_SHIFT_RIGHT
        case kVK_Option:       return GHOSTTY_KEY_ALT_LEFT
        case kVK_RightOption:  return GHOSTTY_KEY_ALT_RIGHT
        case kVK_Control:      return GHOSTTY_KEY_CONTROL_LEFT
        case kVK_RightControl: return GHOSTTY_KEY_CONTROL_RIGHT

        // Control Pad
        case kVK_ForwardDelete: return GHOSTTY_KEY_DELETE       // Fn+Delete on Mac
        case kVK_Home:          return GHOSTTY_KEY_HOME
        case kVK_End:           return GHOSTTY_KEY_END
        case kVK_PageUp:        return GHOSTTY_KEY_PAGE_UP
        case kVK_PageDown:      return GHOSTTY_KEY_PAGE_DOWN
        case kVK_Help:          return GHOSTTY_KEY_HELP

        // Arrow Pad
        case kVK_UpArrow:    return GHOSTTY_KEY_ARROW_UP
        case kVK_DownArrow:  return GHOSTTY_KEY_ARROW_DOWN
        case kVK_LeftArrow:  return GHOSTTY_KEY_ARROW_LEFT
        case kVK_RightArrow: return GHOSTTY_KEY_ARROW_RIGHT

        // Numpad
        case kVK_ANSI_Keypad0:        return GHOSTTY_KEY_NUMPAD_0
        case kVK_ANSI_Keypad1:        return GHOSTTY_KEY_NUMPAD_1
        case kVK_ANSI_Keypad2:        return GHOSTTY_KEY_NUMPAD_2
        case kVK_ANSI_Keypad3:        return GHOSTTY_KEY_NUMPAD_3
        case kVK_ANSI_Keypad4:        return GHOSTTY_KEY_NUMPAD_4
        case kVK_ANSI_Keypad5:        return GHOSTTY_KEY_NUMPAD_5
        case kVK_ANSI_Keypad6:        return GHOSTTY_KEY_NUMPAD_6
        case kVK_ANSI_Keypad7:        return GHOSTTY_KEY_NUMPAD_7
        case kVK_ANSI_Keypad8:        return GHOSTTY_KEY_NUMPAD_8
        case kVK_ANSI_Keypad9:        return GHOSTTY_KEY_NUMPAD_9
        case kVK_ANSI_KeypadDecimal:  return GHOSTTY_KEY_NUMPAD_DECIMAL
        case kVK_ANSI_KeypadMultiply: return GHOSTTY_KEY_NUMPAD_MULTIPLY
        case kVK_ANSI_KeypadPlus:     return GHOSTTY_KEY_NUMPAD_ADD
        case kVK_ANSI_KeypadClear:    return GHOSTTY_KEY_NUMPAD_CLEAR
        case kVK_ANSI_KeypadDivide:   return GHOSTTY_KEY_NUMPAD_DIVIDE
        case kVK_ANSI_KeypadEnter:    return GHOSTTY_KEY_NUMPAD_ENTER
        case kVK_ANSI_KeypadMinus:    return GHOSTTY_KEY_NUMPAD_SUBTRACT
        case kVK_ANSI_KeypadEquals:   return GHOSTTY_KEY_NUMPAD_EQUAL

        // Function Row
        case kVK_F1:  return GHOSTTY_KEY_F1
        case kVK_F2:  return GHOSTTY_KEY_F2
        case kVK_F3:  return GHOSTTY_KEY_F3
        case kVK_F4:  return GHOSTTY_KEY_F4
        case kVK_F5:  return GHOSTTY_KEY_F5
        case kVK_F6:  return GHOSTTY_KEY_F6
        case kVK_F7:  return GHOSTTY_KEY_F7
        case kVK_F8:  return GHOSTTY_KEY_F8
        case kVK_F9:  return GHOSTTY_KEY_F9
        case kVK_F10: return GHOSTTY_KEY_F10
        case kVK_F11: return GHOSTTY_KEY_F11
        case kVK_F12: return GHOSTTY_KEY_F12
        case kVK_F13: return GHOSTTY_KEY_F13
        case kVK_F14: return GHOSTTY_KEY_F14
        case kVK_F15: return GHOSTTY_KEY_F15
        case kVK_F16: return GHOSTTY_KEY_F16
        case kVK_F17: return GHOSTTY_KEY_F17
        case kVK_F18: return GHOSTTY_KEY_F18
        case kVK_F19: return GHOSTTY_KEY_F19
        case kVK_F20: return GHOSTTY_KEY_F20

        default:
            return GHOSTTY_KEY_UNIDENTIFIED
        }
    }
}
