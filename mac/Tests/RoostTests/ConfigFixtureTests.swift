// Load every `tests/config-fixtures/*.json` and run it against
// `Config.swift`'s `parse`. The now-removed GTK UI had a Rust loader
// (`config_fixture_tests.rs`) that ran the same files, so a
// divergence between the two config parsers surfaced on whichever
// side regressed; no equivalent Rust loader exists today
// (`crates/roost-ui-model/src/config.rs`'s own unit tests cover the
// parser directly instead).
//
// Format documented in `tests/config-fixtures/README.md`. Same
// workspace-root path walk as `AgentStateFixtureTests`. Known accepted
// asymmetry (same as the agent-state precedent): Swift `Decodable`
// ignores unknown JSON fields where Rust denies them — the Rust loader
// is the schema gate.

import Foundation
import Testing

@testable import Roost

// MARK: - Case shapes

private struct KeybindExpect: Decodable, Equatable {
    let trigger: String
    let action: String
}

private struct CommandExpect: Decodable, Equatable {
    let label: String
    let run: String
    let title: String
    let hold: Bool
    let env: [[String]]
}

private struct ProviderExpect: Decodable, Equatable {
    let label: String
    let run: String
    let title: String
    let timeoutSecs: UInt64
    let limit: Int

    enum CodingKeys: String, CodingKey {
        case label
        case run
        case title
        case timeoutSecs = "timeout_secs"
        case limit
    }
}

private struct ValueCase: Decodable {
    let name: String
    let content: String
    let expect: Expect

    struct Expect: Decodable {
        let theme: String?
        let fontFamily: String?
        let fontSize: Double?
        let copyOnSelect: String
        let clipboardWrite: String
        let showSidebarAgents: Bool
        /// `nil` = the default extra-word-char set; `""` = explicit empty.
        let wordBreakChars: String?
        let keybinds: [KeybindExpect]
        let commands: [CommandExpect]
        let providers: [ProviderExpect]

        enum CodingKeys: String, CodingKey {
            case theme
            case fontFamily = "font_family"
            case fontSize = "font_size"
            case copyOnSelect = "copy_on_select"
            case clipboardWrite = "clipboard_write"
            case showSidebarAgents = "show_sidebar_agents"
            case wordBreakChars = "word_break_chars"
            case keybinds
            case commands
            case providers
        }
    }
}

// MARK: - File shapes

private struct ConfigValuesFile: Decodable {
    let cases: [ValueCase]
}

private struct GroupProbe: Decodable {
    let group: String
}

private enum FixtureFile {
    case configValues(ConfigValuesFile)

    /// The group name as it appears in the file, so a loader failure
    /// names the same string a reader sees in the JSON.
    var group: String {
        switch self {
        case .configValues: return "config_values"
        }
    }

    var caseCount: Int {
        switch self {
        case .configValues(let f): return f.cases.count
        }
    }
}

// MARK: - Loader

private func fixturesDir() throws -> URL {
    let here = URL(fileURLWithPath: #filePath)
    var root = here
    for _ in 0..<4 {
        root.deleteLastPathComponent()
    }
    let dir = root
        .appendingPathComponent("tests")
        .appendingPathComponent("config-fixtures")
    var isDir: ObjCBool = false
    let exists = FileManager.default.fileExists(atPath: dir.path, isDirectory: &isDir)
    guard exists, isDir.boolValue else {
        throw NSError(
            domain: "ConfigFixtures",
            code: 1,
            userInfo: [
                NSLocalizedDescriptionKey:
                    "fixture dir not found at \(dir.path); did the repo layout change?"
            ]
        )
    }
    return dir
}

private func loadFixtures() throws -> [(file: String, parsed: FixtureFile)] {
    let dir = try fixturesDir()
    let names = try FileManager.default
        .contentsOfDirectory(atPath: dir.path)
        .filter { $0.hasSuffix(".json") }
        .sorted()
    guard !names.isEmpty else {
        throw NSError(
            domain: "ConfigFixtures",
            code: 2,
            userInfo: [NSLocalizedDescriptionKey: "no fixtures in \(dir.path)"]
        )
    }
    let decoder = JSONDecoder()
    return try names.map { name in
        let data = try Data(contentsOf: dir.appendingPathComponent(name))
        let group = try decoder.decode(GroupProbe.self, from: data).group
        let parsed: FixtureFile
        switch group {
        case "config_values":
            parsed = .configValues(try decoder.decode(ConfigValuesFile.self, from: data))
        default:
            throw NSError(
                domain: "ConfigFixtures",
                code: 3,
                userInfo: [NSLocalizedDescriptionKey: "\(name): unknown group \"\(group)\""]
            )
        }
        return (file: name, parsed: parsed)
    }
}

/// Accumulate rather than assert, so one run reports every mismatch in
/// the corpus instead of only the first.
private func check<T: Equatable>(
    _ failures: inout [String],
    _ at: String,
    _ what: String,
    _ got: T,
    _ want: T
) {
    if got != want {
        failures.append("\(at): \(what) got \(got), want \(want)")
    }
}

private func copyOnSelectName(_ v: CopyOnSelect) -> String {
    switch v {
    case .off: return "off"
    case .on: return "true"
    case .clipboard: return "clipboard"
    }
}

private func clipboardWriteName(_ v: ClipboardWrite) -> String {
    switch v {
    case .allow: return "allow"
    case .deny: return "deny"
    }
}

// MARK: - Tests

/// Every group must stay present AND carry real cases. Checking only
/// presence (or only a global case total) lets one group be emptied
/// while the others keep the suite green.
@Test
func everyConfigFixtureGroupIsRepresentedAndNonTrivial() throws {
    // Floors, not targets: set just under the current counts so genuine
    // pruning is allowed but silently gutting a group is not. Kept in
    // step with the Rust loader's table.
    let want: [String: Int] = [
        "config_values": 22
    ]

    var found: [String: Int] = [:]
    for (_, parsed) in try loadFixtures() {
        found[parsed.group, default: 0] += parsed.caseCount
    }

    #expect(Set(found.keys) == Set(want.keys), "missing fixture group(s)")
    for (group, floor) in want.sorted(by: { $0.key < $1.key }) {
        let got = found[group] ?? 0
        #expect(got >= floor, "fixture group \(group) has \(got) case(s), below the \(floor) floor")
    }
}

@Test
func everyConfigFixtureMatchesTheSwiftParser() throws {
    var failures: [String] = []
    var cases = 0

    for (file, parsed) in try loadFixtures() {
        switch parsed {
        case .configValues(let f):
            for c in f.cases {
                cases += 1
                let at = "\(file)[\(c.name)]"
                let cfg = parse(c.content)
                check(&failures, at, "theme", cfg.themeName, c.expect.theme)
                check(&failures, at, "fontFamily", cfg.fontFamily, c.expect.fontFamily)
                check(&failures, at, "fontSize", cfg.fontSize.map(Double.init), c.expect.fontSize)
                check(
                    &failures, at, "copyOnSelect",
                    copyOnSelectName(cfg.copyOnSelect), c.expect.copyOnSelect
                )
                check(
                    &failures, at, "clipboardWrite",
                    clipboardWriteName(cfg.clipboardWrite), c.expect.clipboardWrite
                )
                check(
                    &failures, at, "showSidebarAgents",
                    cfg.showSidebarAgents, c.expect.showSidebarAgents
                )
                let wantWbc = c.expect.wordBreakChars ?? RoostConfig.empty.wordBreakChars
                check(&failures, at, "wordBreakChars", cfg.wordBreakChars, wantWbc)
                let gotKeybinds = cfg.keybinds.map {
                    KeybindExpect(trigger: $0.trigger, action: $0.action)
                }
                check(&failures, at, "keybinds", gotKeybinds, c.expect.keybinds)
                let gotCommands = cfg.commands.map {
                    CommandExpect(
                        label: $0.label, run: $0.run, title: $0.title,
                        hold: $0.hold, env: $0.env.map { [$0.0, $0.1] }
                    )
                }
                check(&failures, at, "commands", gotCommands, c.expect.commands)
                let gotProviders = cfg.providers.map {
                    ProviderExpect(
                        label: $0.label, run: $0.run, title: $0.title,
                        timeoutSecs: $0.timeoutSecs, limit: $0.limit
                    )
                }
                check(&failures, at, "providers", gotProviders, c.expect.providers)
            }
        }
    }

    #expect(cases > 0, "fixtures loaded but contained no cases")
    if !failures.isEmpty {
        let detail = failures.joined(separator: "\n")
        Issue.record("config fixture failures (\(failures.count) of \(cases) cases):\n\(detail)")
    }
}
