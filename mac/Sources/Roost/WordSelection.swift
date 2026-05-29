// Pure word- and line-expansion helpers for double-/triple-click
// selection. Mirrors `crates/roost-linux/src/word_selection.rs` 1:1
// (PR B); the shared `tests/word-fixtures/` corpus pins the two ports
// byte-equal on every supported edge case.
//
// No AppKit imports here — same shape as `UrlDetection.swift` so the
// algorithm can be exercised from a swift-testing target without
// constructing a TerminalView.
//
// **Word-char definition.** A character is a "word char" if it's a
// Unicode letter, a Unicode digit, OR appears in the configured
// `extraWordChars` set (default `_-.+~/:@%`, matching Ghostty). Every
// other character is a break char. Counter-intuitive consequence: `/`
// and `.` are word chars by default, so `/tmp/foo.txt` and
// `https://example.com` select as a single unit on double-click —
// what every other terminal does (iTerm2, gnome-terminal, kitty,
// Terminal.app).
//
// **Indexing.** Inputs and outputs are indexed by Unicode scalar
// (codepoint), NOT by Swift `Character` (grapheme cluster), matching
// `UrlDetection.find` and the Rust port's `chars()`. Combining marks
// like `e\u{0301}` therefore count as 2 columns — that's what keeps
// the cross-port fixture corpus byte-equal.

import Foundation

/// One word- or line-span inside a terminal row. Both ends are
/// inclusive scalar-indexed columns. Same shape + semantics as
/// `roost_linux::word_selection::WordSpan` on the Rust side.
struct WordSpan: Equatable {
    let col0: Int
    let col1: Int
}

enum WordSelection {
    /// Default extra-word-char set: chars that count as word chars
    /// beyond Unicode letters/digits, so file paths (`/tmp/foo.txt`),
    /// URLs (`https://example.com`), and identifiers
    /// (`my_var-name`) select as single units on double-click.
    /// Mirrors ghostty's default and what cmux+iTerm2 ship.
    static let defaultWordChars: String = "_-.+~/:@%"

    /// Expand `(col)` outward to the surrounding word in `row`.
    /// Returns `nil` when the clicked cell itself is whitespace
    /// (caller falls through to single-cell selection — iTerm2 has a
    /// "select previous word" alternative we deliberately don't
    /// implement; nil here is the simpler, more predictable contract).
    ///
    /// `col` is a 0-indexed Unicode scalar position; out-of-range
    /// values return `nil`.
    static func expandWord(
        in row: String,
        at col: Int,
        extraWordChars: String = defaultWordChars
    ) -> WordSpan? {
        let scalars = Array(row.unicodeScalars)
        guard col >= 0, col < scalars.count else { return nil }
        let extras = Set(extraWordChars.unicodeScalars)
        if !isWordScalar(scalars[col], extras: extras) {
            return nil
        }
        var c0 = col
        while c0 > 0, isWordScalar(scalars[c0 - 1], extras: extras) {
            c0 -= 1
        }
        var c1 = col
        while c1 + 1 < scalars.count, isWordScalar(scalars[c1 + 1], extras: extras) {
            c1 += 1
        }
        return WordSpan(col0: c0, col1: c1)
    }

    /// Full visible-row span for triple-click. Returns
    /// `(0, lastNonBlankCol)` where blank = ASCII 0x20. A fully-blank
    /// row returns `(0, 0)` — degenerate but well-defined; the caller
    /// (`setSelection`) clamps it to a single-cell highlight, same as
    /// ghostty's triple-click on an empty row.
    static func expandLine(in row: String) -> WordSpan {
        let scalars = Array(row.unicodeScalars)
        guard !scalars.isEmpty else { return WordSpan(col0: 0, col1: 0) }
        let space: Unicode.Scalar = " "
        var idx = scalars.count - 1
        while idx > 0, scalars[idx] == space {
            idx -= 1
        }
        return WordSpan(col0: 0, col1: idx)
    }

    /// A Unicode scalar is a "word scalar" if it's a Unicode letter,
    /// a Unicode digit, or appears in the configured `extras` set.
    /// `Unicode.Scalar.properties.{isAlphabetic,generalCategory}`
    /// mirrors Rust's `char::is_alphabetic() || char::is_numeric()`,
    /// keeping the cross-port behavior byte-equal across the
    /// fixture corpus.
    private static func isWordScalar(
        _ s: Unicode.Scalar,
        extras: Set<Unicode.Scalar>
    ) -> Bool {
        if extras.contains(s) { return true }
        if s.properties.isAlphabetic { return true }
        switch s.properties.generalCategory {
        case .decimalNumber, .letterNumber, .otherNumber:
            return true
        default:
            return false
        }
    }
}
