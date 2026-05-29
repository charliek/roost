// Load every `tests/url-fixtures/*.txt` and assert byte-exact agreement
// with the Swift `UrlDetection` port. Same loader the Rust crate runs
// against in `crates/roost-url/src/lib.rs::fixtures`. Drift between
// the Rust regex and the Swift mirror surfaces as a failure here.
//
// Same pattern as `tests/ipc-vectors/`: both ports consume the same
// fixture bytes, so a corpus drift would surface immediately on
// either side.

import Foundation
import Testing

@testable import Roost

@Test
func everyUrlFixtureRoundTrips() throws {
    let dir = try fixturesDir()
    let fm = FileManager.default
    let contents = try fm.contentsOfDirectory(atPath: dir.path)
    let txtFiles = contents.filter { $0.hasSuffix(".txt") }.sorted()
    #expect(!txtFiles.isEmpty, "no fixtures at \(dir.path)")
    var failures: [String] = []
    for filename in txtFiles {
        let path = dir.appendingPathComponent(filename)
        guard let fixture = try? parseFixture(at: path) else {
            failures.append("[\(filename)] failed to parse fixture")
            continue
        }
        let got = UrlDetection.find(in: fixture.row, at: fixture.col)
        switch (fixture.want, got) {
        case (nil, nil):
            continue
        case (nil, let g?):
            failures.append("[\(filename)] expected no match, got \(g)")
        case (_?, nil):
            failures.append("[\(filename)] expected match, got nil")
        case let (want?, g?):
            if g.col0 != want.col0 || g.col1 != want.col1 || g.url != want.url {
                failures.append(
                    "[\(filename)] mismatch: got col0=\(g.col0) col1=\(g.col1) url=\(g.url), want col0=\(want.col0) col1=\(want.col1) url=\(want.url)"
                )
            }
        }
    }
    if !failures.isEmpty {
        Issue.record("fixture failures:\n\(failures.joined(separator: "\n"))")
    }
}

private struct Fixture {
    let row: String
    let col: Int
    let want: (col0: Int, col1: Int, url: String)?
}

private func parseFixture(at url: URL) throws -> Fixture {
    let raw = try String(contentsOf: url, encoding: .utf8)
    var row: String?
    var col: Int?
    var col0: Int?
    var col1: Int?
    var urlStr: String?
    var afterSep = false
    for line in raw.components(separatedBy: "\n") {
        if line.hasPrefix("#") || line.isEmpty { continue }
        if line == "---" { afterSep = true; continue }
        guard let sepRange = line.range(of: ": ") else { continue }
        let key = String(line[..<sepRange.lowerBound])
        let value = String(line[sepRange.upperBound...])
        switch key {
        case "row" where !afterSep:  row = value
        case "col" where !afterSep:  col = Int(value)
        case "col0" where afterSep:  col0 = Int(value)
        case "col1" where afterSep:  col1 = Int(value)
        case "url" where afterSep:   urlStr = value
        default: continue
        }
    }
    let want: (Int, Int, String)?
    if let c0 = col0, let c1 = col1, let u = urlStr {
        want = (c0, c1, u)
    } else {
        want = nil
    }
    return Fixture(row: row ?? "", col: col ?? 0, want: want)
}

/// Walk upward from this source file to find `tests/url-fixtures/` at
/// the workspace root. Swift test binaries don't carry CARGO_MANIFEST_DIR
/// equivalents; the source-file relative walk matches what the Rust
/// fixture loader does via `env!("CARGO_MANIFEST_DIR")`.
private func fixturesDir() throws -> URL {
    // `#filePath` resolves to this source file at compile time.
    let here = URL(fileURLWithPath: #filePath)
    // mac/Tests/RoostTests/UrlFixtureRoundTripTests.swift → repo root
    // is 4 levels up.
    var root = here
    for _ in 0..<4 {
        root.deleteLastPathComponent()
    }
    let dir = root.appendingPathComponent("tests").appendingPathComponent("url-fixtures")
    // Validate — a clearer error than "no fixtures found" if the path
    // walk drifts after a future repo reshuffle.
    var isDir: ObjCBool = false
    let exists = FileManager.default.fileExists(atPath: dir.path, isDirectory: &isDir)
    guard exists, isDir.boolValue else {
        throw NSError(
            domain: "UrlFixtureRoundTrip",
            code: 1,
            userInfo: [
                NSLocalizedDescriptionKey:
                    "fixture dir not found at \(dir.path); did the repo layout change?"
            ]
        )
    }
    return dir
}
