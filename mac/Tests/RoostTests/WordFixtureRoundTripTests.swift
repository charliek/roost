// Load every `tests/word-fixtures/*.txt` and assert byte-exact agreement
// with the Swift `WordSelection` port. The Rust loader in PR B runs
// against the same files; drift between the two ports surfaces as a
// failure on whichever side regressed.
//
// Same shape as `UrlFixtureRoundTripTests.swift`.

import Foundation
import Testing

@testable import Roost

@Test
func everyWordFixtureRoundTrips() throws {
    let dir = try wordFixturesDir()
    let fm = FileManager.default
    let contents = try fm.contentsOfDirectory(atPath: dir.path)
    let txtFiles = contents.filter { $0.hasSuffix(".txt") }.sorted()
    #expect(!txtFiles.isEmpty, "no fixtures at \(dir.path)")
    var failures: [String] = []
    for filename in txtFiles {
        let path = dir.appendingPathComponent(filename)
        let fixture: WordFixture
        do {
            fixture = try parseWordFixture(at: path)
        } catch {
            failures.append("[\(filename)] failed to parse: \(error)")
            continue
        }
        let got: WordSpan?
        let gotText: String?
        switch fixture.clickCount {
        case 2:
            got = WordSelection.expandWord(
                in: fixture.row,
                at: fixture.col,
                extraWordChars: fixture.breakChars
            )
            gotText = got.map { sliceScalars(fixture.row, $0.col0, $0.col1) }
        case 3:
            let span = WordSelection.expandLine(in: fixture.row)
            got = span
            gotText = sliceScalars(fixture.row, span.col0, span.col1)
        default:
            failures.append("[\(filename)] invalid click_count \(fixture.clickCount)")
            continue
        }
        switch (fixture.want, got) {
        case (nil, nil):
            continue
        case (nil, let g?):
            failures.append("[\(filename)] expected no match, got \(g) (text=\(gotText ?? "")")
        case (_?, nil):
            failures.append("[\(filename)] expected match, got nil")
        case let (want?, g?):
            if g.col0 != want.col0 || g.col1 != want.col1 {
                failures.append(
                    "[\(filename)] span mismatch: got col0=\(g.col0) col1=\(g.col1) text=\(gotText ?? ""), want col0=\(want.col0) col1=\(want.col1) text=\(want.text)"
                )
            } else if let gt = gotText, gt != want.text {
                failures.append(
                    "[\(filename)] text mismatch: got \"\(gt)\", want \"\(want.text)\""
                )
            }
        }
    }
    if !failures.isEmpty {
        Issue.record("fixture failures:\n\(failures.joined(separator: "\n"))")
    }
}

private struct WordFixture {
    let row: String
    let col: Int
    let breakChars: String
    let clickCount: Int
    let want: (col0: Int, col1: Int, text: String)?
}

/// Slice scalars `[c0, c1]` inclusive from `row`. The expected `text`
/// in a fixture is the substring the span covers, so we re-derive it
/// here for the diagnostic message.
private func sliceScalars(_ row: String, _ c0: Int, _ c1: Int) -> String {
    let scalars = Array(row.unicodeScalars)
    guard c0 >= 0, c1 < scalars.count, c0 <= c1 else { return "" }
    var out = String.UnicodeScalarView()
    for i in c0...c1 {
        out.append(scalars[i])
    }
    return String(out)
}

/// Parser for the fixture format documented in
/// `tests/word-fixtures/README.md`. Lenient on blank lines and
/// `#`-comments; strict on partial expected blocks (a fixture with
/// `col0` but no `col1` is a typo, not a "no match").
///
/// The `row:` line preserves trailing whitespace by hand (the format
/// embeds literal trailing spaces in fixture 07 — the `expandLine`
/// trim is what we're testing). `text:` lines also preserve trailing
/// whitespace for symmetry, though no current fixture needs it.
private func parseWordFixture(at url: URL) throws -> WordFixture {
    let raw = try String(contentsOf: url, encoding: .utf8)
    var row: String?
    var col: Int?
    var breakChars: String?
    var clickCount: Int?
    var col0: Int?
    var col1: Int?
    var text: String?
    var afterSep = false
    for line in raw.components(separatedBy: "\n") {
        if line.hasPrefix("#") { continue }
        if line.isEmpty { continue }
        if line == "---" {
            afterSep = true
            continue
        }
        guard let sepRange = line.range(of: ": ") else { continue }
        let key = String(line[..<sepRange.lowerBound])
        // Preserve trailing whitespace — fixture 07 has 5 trailing
        // spaces on the `row:` line, and `expandLine`'s job is to peel
        // them. Trimming here would defeat the test.
        let value = String(line[sepRange.upperBound...])
        switch key {
        case "row" where !afterSep: row = value
        case "col" where !afterSep: col = Int(value.trimmingCharacters(in: .whitespaces))
        case "break_chars" where !afterSep:
            breakChars = value.trimmingCharacters(in: .whitespaces)
        case "click_count" where !afterSep:
            clickCount = Int(value.trimmingCharacters(in: .whitespaces))
        case "col0" where afterSep: col0 = Int(value.trimmingCharacters(in: .whitespaces))
        case "col1" where afterSep: col1 = Int(value.trimmingCharacters(in: .whitespaces))
        case "text" where afterSep: text = value
        default: continue
        }
    }
    let want: (Int, Int, String)?
    switch (col0, col1, text) {
    case (.some(let c0), .some(let c1), .some(let t)):
        want = (c0, c1, t)
    case (nil, nil, nil):
        want = nil
    default:
        throw NSError(
            domain: "WordFixtureRoundTrip",
            code: 2,
            userInfo: [
                NSLocalizedDescriptionKey:
                    "partial expected block in \(url.lastPathComponent) (col0=\(String(describing: col0)), col1=\(String(describing: col1)), text=\(String(describing: text))) — supply all three or none"
            ]
        )
    }
    return WordFixture(
        row: row ?? "",
        col: col ?? 0,
        breakChars: breakChars ?? WordSelection.defaultWordChars,
        clickCount: clickCount ?? 2,
        want: want
    )
}

/// Walk upward from this source file to find `tests/word-fixtures/` at
/// the workspace root — same scheme as `UrlFixtureRoundTripTests`.
private func wordFixturesDir() throws -> URL {
    let here = URL(fileURLWithPath: #filePath)
    var root = here
    for _ in 0..<4 {
        root.deleteLastPathComponent()
    }
    let dir = root.appendingPathComponent("tests").appendingPathComponent("word-fixtures")
    var isDir: ObjCBool = false
    let exists = FileManager.default.fileExists(atPath: dir.path, isDirectory: &isDir)
    guard exists, isDir.boolValue else {
        throw NSError(
            domain: "WordFixtureRoundTrip",
            code: 1,
            userInfo: [
                NSLocalizedDescriptionKey:
                    "fixture dir not found at \(dir.path); did the repo layout change?"
            ]
        )
    }
    return dir
}
