// URL detection mirror of `crates/roost-url/src/lib.rs`. The Rust
// crate is authoritative; this file reimplements the same regex +
// trim pipeline by hand because the AppKit UI doesn't link the Rust
// workspace. Parity is pinned by the shared `tests/url-fixtures/`
// corpus — both UIs load the same files and assert byte-equality.
//
// The regex is a scheme-only pattern. Path detection (./foo, ~/bar)
// and scp-style git remotes (git@github.com:x/y) are out of scope
// for now; widen here + in `roost-url` together when the time comes.

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
    /// Compiled URL regex. The body's exclusion set rejects only the
    /// whitespace + delimiter characters spelled out below and accepts
    /// every other codepoint (so IDNA hosts like `https://例え.テスト`
    /// match the body class as-is).
    ///
    /// The body's negation class spells out `[\t\n\x0c\r ]` instead of
    /// `\s` because `NSRegularExpression`'s ICU regex treats `\s` as
    /// Unicode whitespace (matches U+00A0 NBSP, U+2028, U+2029, etc.).
    /// The explicit class is the only way to keep parity with the Rust
    /// implementation in `roost-url`.
    ///
    /// Pattern is byte-identical with `roost_url::pattern()` to keep
    /// the two implementations honest. Use Swift's raw-string literal
    /// `#"..."#` so the embedded backticks + backslash don't need
    /// escaping.
    static let pattern: NSRegularExpression = {
        let raw = #"(?:(?:https?|ftp|file|ssh|git\+ssh)://|(?:mailto|tel|news):)[^ \t\r\n\x0c<>"'`\\]+"#
        // `.caseInsensitive` corresponds to `(?i)` in the Rust regex —
        // dropped the inline flag from the pattern so the same body
        // text is shared with the Rust side.
        return try! NSRegularExpression(pattern: raw, options: [.caseInsensitive])
    }()

    /// Find the URL straddling column `col` in `row`, or `nil` if no
    /// URL covers that column. `col` is a 0-indexed Unicode scalar
    /// position into `row` (i.e. codepoint, matching Rust's `chars()`)
    /// — out-of-range values return `nil`.
    ///
    /// Indexing by scalar (rather than grapheme cluster) is what keeps
    /// us byte-exact with `roost_url::find_url_at` on cases like
    /// `e\u{0301}` (e + combining acute), which Rust counts as 2 chars
    /// and Swift's `Character` would have collapsed to 1.
    ///
    /// Mirrors `roost_url::find_url_at` 1:1.
    static func find(in row: String, at col: Int) -> UrlSpan? {
        let totalScalars = row.unicodeScalars.count
        guard col >= 0, col < totalScalars else { return nil }
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
        // UTF-16 → scalar-index (= column) table so we can translate
        // match ranges back. Each entry holds the scalar index whose
        // UTF-16 span starts at that UTF-16 offset; the final entry
        // holds the total scalar count so end-of-match lookups don't
        // need a special case.
        //
        // Indexing by Unicode scalar (codepoint), NOT Swift `Character`
        // (grapheme cluster), is what keeps this byte-exact with the
        // Rust implementation's `chars()`. Combining marks like
        // `e\u{0301}` therefore count as 2 columns here, same as Rust
        // — fixtures with combining sequences would otherwise drift
        // between the two UIs.
        var utf16ToScalar: [Int] = []
        utf16ToScalar.reserveCapacity(row.utf16.count + 1)
        var scalarIdx = 0
        for s in row.unicodeScalars {
            // Each Unicode scalar may span one or two UTF-16 code
            // units (BMP = 1, supplementary plane = 2 via surrogate
            // pair). Record the scalar index across that whole span.
            let units = String(s).utf16.count
            for _ in 0..<units {
                utf16ToScalar.append(scalarIdx)
            }
            scalarIdx += 1
        }
        utf16ToScalar.append(scalarIdx)

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
            guard startUtf16 < utf16ToScalar.count, endUtf16 < utf16ToScalar.count else { continue }
            let col0 = utf16ToScalar[startUtf16]
            let endScalarExclusive = utf16ToScalar[endUtf16]
            guard endScalarExclusive > col0 else { continue }
            let col1 = endScalarExclusive - 1
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
        // `UInt16(exactly:)` / `UInt32(exactly:)` guard against trap on
        // an out-of-range Int — the caller's docstring contract is
        // "out-of-range returns nil", not "crash".
        guard
            let col16 = UInt16(exactly: col),
            let row32 = UInt32(exactly: row)
        else { return nil }
        var pt = GhosttyPoint()
        pt.tag = GHOSTTY_POINT_TAG_VIEWPORT
        pt.value.coordinate.x = col16
        pt.value.coordinate.y = row32
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
    /// brackets from a URL match. Mirrors `roost_url::trim_url`:
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
