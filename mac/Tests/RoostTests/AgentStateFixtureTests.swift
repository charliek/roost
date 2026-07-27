// Load every `tests/agent-state-fixtures/*.json` and run it against
// `AgentState.swift`. The Rust loader in
// `crates/roost-ipc/tests/agent_state_fixtures.rs` runs the same files,
// so a divergence between the two ports of the agent state machine
// surfaces on whichever side regressed.
//
// Format documented in `tests/agent-state-fixtures/README.md`. Same
// workspace-root path walk as `WordFixtureRoundTripTests`.

import Foundation
import Testing

@testable import Roost

// MARK: - Case shapes

private struct DerivationCase: Decodable {
    let name: String
    let state: AgentTabState
    let expect: Expect

    struct Expect: Decodable {
        let effective: Workspace.TabState
        let isLive: Bool
        let suppressRawOsc: Bool

        enum CodingKeys: String, CodingKey {
            case effective
            case isLive = "is_live"
            case suppressRawOsc = "suppress_raw_osc"
        }
    }
}

private struct RankCase: Decodable {
    let name: String
    let lifecycle: AgentLifecycle
    let expect: Expect

    struct Expect: Decodable {
        let rank: Int
    }
}

private struct TransitionCase: Decodable {
    let name: String
    let now: Int64
    let current: AgentTabState
    let report: AgentReport
    let expect: Expect

    struct Expect: Decodable {
        let accepted: Bool
        let ownershipChanged: Bool
        let lifecycleChanged: Bool
        let attention: AttentionEffect
        let state: AgentTabState
        let effective: Workspace.TabState

        enum CodingKeys: String, CodingKey {
            case accepted
            case ownershipChanged = "ownership_changed"
            case lifecycleChanged = "lifecycle_changed"
            case attention
            case state
            case effective
        }
    }
}

private struct ShellMarkCase: Decodable {
    let name: String
    let state: AgentTabState
    let body: String
    let expect: Expect

    struct Expect: Decodable {
        /// `false` means the mark is undefined and the state is untouched.
        let changed: Bool
        let shell: ShellState
        let lifecycle: AgentLifecycle
        let ownerRetained: Bool

        enum CodingKeys: String, CodingKey {
            case changed
            case shell
            case lifecycle
            case ownerRetained = "owner_retained"
        }
    }
}

// MARK: - File shapes

private struct DerivationFile: Decodable {
    let cases: [DerivationCase]
}

private struct RankFile: Decodable {
    let orderHighToLow: [AgentLifecycle]
    let cases: [RankCase]

    enum CodingKeys: String, CodingKey {
        case orderHighToLow = "order_high_to_low"
        case cases
    }
}

private struct TransitionsFile: Decodable {
    let cases: [TransitionCase]
}

private struct ShellMarksFile: Decodable {
    let cases: [ShellMarkCase]
}

private struct GroupProbe: Decodable {
    let group: String
}

private enum FixtureFile {
    case derivation(DerivationFile)
    case rank(RankFile)
    case transitions(TransitionsFile)
    case shellMarks(ShellMarksFile)

    /// The group name as it appears in the file, so a loader failure
    /// names the same string a reader sees in the JSON.
    var group: String {
        switch self {
        case .derivation: return "derivation"
        case .rank: return "rank"
        case .transitions: return "transitions"
        case .shellMarks: return "shell_marks"
        }
    }

    var caseCount: Int {
        switch self {
        case .derivation(let f): return f.cases.count
        case .rank(let f): return f.cases.count
        case .transitions(let f): return f.cases.count
        case .shellMarks(let f): return f.cases.count
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
        .appendingPathComponent("agent-state-fixtures")
    var isDir: ObjCBool = false
    let exists = FileManager.default.fileExists(atPath: dir.path, isDirectory: &isDir)
    guard exists, isDir.boolValue else {
        throw NSError(
            domain: "AgentStateFixtures",
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
            domain: "AgentStateFixtures",
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
        case "derivation":
            parsed = .derivation(try decoder.decode(DerivationFile.self, from: data))
        case "rank":
            parsed = .rank(try decoder.decode(RankFile.self, from: data))
        case "transitions":
            parsed = .transitions(try decoder.decode(TransitionsFile.self, from: data))
        case "shell_marks":
            parsed = .shellMarks(try decoder.decode(ShellMarksFile.self, from: data))
        default:
            throw NSError(
                domain: "AgentStateFixtures",
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

// MARK: - Tests

/// Every group must stay present AND carry real cases. Checking only
/// presence (or only a global case total) lets one group be emptied
/// while the others keep the suite green — the whole corpus would then
/// pass while testing nothing on that axis.
@Test
func everyAgentStateFixtureGroupIsRepresentedAndNonTrivial() throws {
    // Floors, not targets: set just under the current counts so genuine
    // pruning is allowed but silently gutting a group is not. Kept in
    // step with the Rust loader's table.
    let want: [String: Int] = [
        "derivation": 12,
        "rank": 5,
        "transitions": 12,
        "shell_marks": 7,
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
func everyAgentStateFixtureMatchesTheSwiftImplementation() throws {
    var failures: [String] = []
    var cases = 0

    for (file, parsed) in try loadFixtures() {
        switch parsed {
        case .derivation(let f):
            for c in f.cases {
                cases += 1
                let at = "\(file)[\(c.name)]"
                check(&failures, at, "isLive", Agent.isLive(c.state), c.expect.isLive)
                check(&failures, at, "effective", Agent.effective(c.state), c.expect.effective)
                check(
                    &failures, at, "suppressRawOsc",
                    Agent.suppressRawOsc(c.state), c.expect.suppressRawOsc
                )
            }
        case .rank(let f):
            for c in f.cases {
                cases += 1
                check(&failures, "\(file)[\(c.name)]", "rank", Agent.rank(c.lifecycle), c.expect.rank)
            }
            for (hi, lo) in zip(f.orderHighToLow, f.orderHighToLow.dropFirst()) {
                if Agent.rank(hi) <= Agent.rank(lo) {
                    failures.append(
                        "\(file)[order]: rank(\(hi))=\(Agent.rank(hi)) must exceed rank(\(lo))=\(Agent.rank(lo))"
                    )
                }
            }
        case .transitions(let f):
            for c in f.cases {
                cases += 1
                let at = "\(file)[\(c.name)]"
                let out = Agent.applyReport(c.current, c.report, now: c.now)
                check(&failures, at, "accepted", out.accepted, c.expect.accepted)
                check(
                    &failures, at, "ownershipChanged",
                    out.ownershipChanged, c.expect.ownershipChanged
                )
                check(
                    &failures, at, "lifecycleChanged",
                    out.lifecycleChanged, c.expect.lifecycleChanged
                )
                check(&failures, at, "attention", out.attention, c.expect.attention)
                check(&failures, at, "state", out.state, c.expect.state)
                check(&failures, at, "effective", Agent.effective(out.state), c.expect.effective)
                if !c.expect.accepted && out.state != c.current {
                    failures.append(
                        "\(at): a dropped report must leave state untouched, got \(out.state)"
                    )
                }
            }
        case .shellMarks(let f):
            for c in f.cases {
                cases += 1
                let at = "\(file)[\(c.name)]"
                let got = Agent.applyShellMark(c.state, body: c.body)
                check(&failures, at, "changed", got != nil, c.expect.changed)
                let after = got ?? c.state
                check(&failures, at, "shell", after.shell, c.expect.shell)
                check(&failures, at, "lifecycle", after.lifecycle, c.expect.lifecycle)
                check(
                    &failures, at, "ownerRetained",
                    after.ownership != nil, c.expect.ownerRetained
                )
            }
        }
    }

    #expect(cases > 0, "fixtures loaded but contained no cases")
    if !failures.isEmpty {
        let detail = failures.joined(separator: "\n")
        Issue.record("agent-state fixture failures (\(failures.count) of \(cases) cases):\n\(detail)")
    }
}
