// Load `tests/agent-display-name-fixtures/cases.json` and run it
// against `AgentPalette.agentDisplayName`. The Rust loader in
// `crates/roost-ui-model/tests/agent_display_name_fixtures_test.rs`
// runs the same file, so a divergence between the two UIs'
// source→display-name tables surfaces on whichever side regressed.
//
// Format documented in `tests/agent-display-name-fixtures/README.md`.
// Same workspace-root path walk as `AgentStateFixtureTests`.

import Foundation
import Testing

@testable import Roost

private struct Case: Decodable {
    let name: String
    let source: String
    let expect: String
}

private struct CasesFile: Decodable {
    let cases: [Case]
}

private func fixturesFile() throws -> URL {
    let here = URL(fileURLWithPath: #filePath)
    var root = here
    for _ in 0..<4 {
        root.deleteLastPathComponent()
    }
    return root
        .appendingPathComponent("tests")
        .appendingPathComponent("agent-display-name-fixtures")
        .appendingPathComponent("cases.json")
}

@Test
func everyAgentDisplayNameFixtureMatchesTheSwiftImplementation() throws {
    let url = try fixturesFile()
    let data = try Data(contentsOf: url)
    let file = try JSONDecoder().decode(CasesFile.self, from: data)
    #expect(!file.cases.isEmpty, "fixture file loaded but contained no cases")

    var failures: [String] = []
    for c in file.cases {
        let got = AgentPalette.agentDisplayName(c.source)
        if got != c.expect {
            failures.append(
                "[\(c.name)] agentDisplayName(\(c.source)) = \(got), want \(c.expect)")
        }
    }
    if !failures.isEmpty {
        Issue.record("fixture failures:\n\(failures.joined(separator: "\n"))")
    }
}
