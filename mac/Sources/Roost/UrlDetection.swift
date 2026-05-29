// URL detection mirror of `crates/roost-url/src/lib.rs`. The Rust
// crate is authoritative; this file ports the same regex + trim
// pipeline by hand because the AppKit UI doesn't link the Rust
// workspace. Parity is pinned by the shared `tests/url-fixtures/`
// corpus — both ports load the same files and assert byte-equality.
//
// The regex is the legacy Go binary's scheme-only pattern (see
// `internal/links/regex.go:18`). Path detection (./foo, ~/bar) and
// scp-style git remotes (git@github.com:x/y) are out of scope for the
// v1 port; widen here + in `roost-url` together when the time comes.

import CGhosttyVT
import Foundation

/// One URL match within a terminal row. `col0`/`col1` are 0-indexed
/// char positions into the row string, both inclusive. Same shape +
/// semantics as `roost_url::UrlSpan` on the Rust side.
struct UrlSpan: Equatable {
    let col0: Int
    let col1: Int
    let url: String
}

enum UrlDetection {
    /// Compiled URL regex. NSRegularExpression's `[]` byte-class
    /// semantics handle the body's exclusion set (`[^\s<>"'`\\]+`)
    /// the same way Go's `regexp` does — both reject the same
    /// characters and accept every other codepoint (so IDNA hosts
    /// like `https://例え.テスト` match the body class as-is).
    ///
    /// Pattern is byte-identical with `roost_url::pattern()` to keep
    /// the two ports honest. Use Swift's raw-string literal `#"..."#`
    /// so the embedded backticks + backslash don't need escaping.
    static let pattern: NSRegularExpression = {
        let raw = #"(?:(?:https?|ftp|file|ssh|git\+ssh)://|(?:mailto|tel|news):)[^\s<>"'`\\]+"#
        // `.caseInsensitive` corresponds to `(?i)` in the Rust regex —
        // dropped the inline flag from the pattern so the same body
        // text is shared with the Rust side.
        return try! NSRegularExpression(pattern: raw, options: [.caseInsensitive])
    }()

    /// Find the URL straddling column `col` in `row`, or `nil` if no
    /// URL covers that column. `col` is a 0-indexed char position into
    /// `row` — out-of-range values return `nil`.
    ///
    /// Mirrors `roost_url::find_url_at` 1:1.
    static func find(in row: String, at col: Int) -> UrlSpan? {
        let chars = Array(row)
        guard col >= 0, col < chars.count else { return nil }
        for span in findAll(in: row) {
            if col >= span.col0 && col <= span.col1 {
                return span
            }
        }
        return nil
    }

    /// Find every URL in `row`, sorted left-to-right.
    static func findAll(in row: String) -> [UrlSpan] {
        // NSRegularExpression operates on UTF-16 code units; build a
        // UTF-16 → Character (= column) index table so we can translate
        // match ranges back. Each entry holds the UTF-16 offset where
        // its char index starts; the final entry holds the total
        // UTF-16 length so end-of-match lookups don't need a special
        // case. Same idea as Go's byteToRune table.
        var utf16ToChar: [Int] = []
        utf16ToChar.reserveCapacity(row.utf16.count + 1)
        var charIdx = 0
        for ch in row {
            // Each Character may span one or more UTF-16 units; the
            // table records the *start* of that span as `charIdx`.
            for _ in 0..<ch.utf16.count {
                utf16ToChar.append(charIdx)
            }
            charIdx += 1
        }
        utf16ToChar.append(charIdx)

        var out: [UrlSpan] = []
        let nsrow = row as NSString
        let full = NSRange(location: 0, length: nsrow.length)
        let matches = pattern.matches(in: row, options: [], range: full)
        for m in matches {
            let r = m.range
            guard r.location != NSNotFound else { continue }
            let raw = nsrow.substring(with: r)
            let trimmed = trimURL(raw)
            if trimmed.isEmpty { continue }
            let trimmedLen = (trimmed as NSString).length
            let startUtf16 = r.location
            let endUtf16 = startUtf16 + trimmedLen
            guard startUtf16 < utf16ToChar.count, endUtf16 < utf16ToChar.count else { continue }
            let col0 = utf16ToChar[startUtf16]
            let endCharExclusive = utf16ToChar[endUtf16]
            guard endCharExclusive > col0 else { continue }
            let col1 = endCharExclusive - 1
            out.append(UrlSpan(col0: col0, col1: col1, url: trimmed))
        }
        return out
    }

    /// Return the OSC 8 hyperlink URI attached to the cell at viewport
    /// coordinates `(col, row)` in the given terminal, or `nil` if the
    /// cell has no explicit hyperlink. Out-of-range coordinates also
    /// return `nil`.
    ///
    /// Mirrors `roost_vt::Terminal::hyperlink_at`: two-call buffer
    /// pattern around `ghostty_grid_ref_hyperlink_uri` so we don't have
    /// to guess at the URI length. The grid_ref is captured + consumed
    /// immediately so libghostty's "valid only until next mutating
    /// call" contract isn't a concern for callers.
    static func hyperlinkAt(terminal: GhosttyTerminal, col: Int, row: Int) -> String? {
        guard col >= 0, row >= 0 else { return nil }
        var pt = GhosttyPoint()
        pt.tag = GHOSTTY_POINT_TAG_VIEWPORT
        pt.value.coordinate.x = UInt16(col)
        pt.value.coordinate.y = UInt32(row)
        var gref = GhosttyGridRef()
        gref.size = MemoryLayout<GhosttyGridRef>.size
        guard ghostty_terminal_grid_ref(terminal, pt, &gref) == GHOSTTY_SUCCESS else {
            return nil
        }
        // First call: null buffer probes the URI length.
        var outLen: Int = 0
        let probe = ghostty_grid_ref_hyperlink_uri(&gref, nil, 0, &outLen)
        if probe == GHOSTTY_SUCCESS {
            // Success + out_len=0 → no hyperlink on this cell.
            return nil
        }
        if probe != GHOSTTY_OUT_OF_SPACE {
            return nil
        }
        if outLen == 0 {
            return nil
        }
        // Second call: allocate + fill.
        var buf = [UInt8](repeating: 0, count: outLen)
        let written = buf.withUnsafeMutableBufferPointer { ptr -> Int? in
            var w: Int = 0
            let rc = ghostty_grid_ref_hyperlink_uri(&gref, ptr.baseAddress, ptr.count, &w)
            return rc == GHOSTTY_SUCCESS ? w : nil
        }
        guard let n = written else { return nil }
        return String(bytes: buf.prefix(n), encoding: .utf8)
    }

    /// Strip trailing sentence punctuation and unmatched closing
    /// brackets from a URL match. Mirrors
    /// `roost_url::trim_url` / `internal/links/regex.go::trimURL`:
    /// sentence punctuation almost never belongs to a URL, and a
    /// trailing `)` is part of the URL only if there's a matching
    /// `(` inside it (so Wikipedia's `_(disambiguation)` survives but
    /// `(see foo)` peels).
    static func trimURL(_ input: String) -> String {
        var u = input
        // Pass 1: strip ASCII trailing punctuation.
        while let last = u.last, ".,;:!?".contains(last) {
            u.removeLast()
        }
        // Pass 2: balance trailing unmatched closing brackets.
        while let last = u.last {
            let (open, close): (Character, Character)
            switch last {
            case ")": (open, close) = ("(", ")")
            case "]": (open, close) = ("[", "]")
            case "}": (open, close) = ("{", "}")
            default: return u
            }
            let opens = u.reduce(into: 0) { acc, c in if c == open { acc += 1 } }
            let closes = u.reduce(into: 0) { acc, c in if c == close { acc += 1 } }
            if closes > opens {
                u.removeLast()
            } else {
                return u
            }
        }
        return u
    }
}
