// IPCHandlerTests — dispatch-level coverage for IPCHandlerImpl.
//
// The Rust handler has tests/ipc_dispatch.rs; the Mac handler had no
// equivalent, leaving its hand-written cross-cutting logic untested:
// strict unknown-field rejection (decodeParams), ipcDim u16
// validation, mapWorkspace/mapPty error-code mapping, the
// not-implemented / unknown-op paths, and result encoding. The two
// handlers must stay behaviorally convergent over the shared wire
// contract, so this suite guards that.
//
// It calls `IPCHandlerImpl.handle(op:params:)` directly — no socket
// needed. It deliberately exercises only NON-PTY-spawning ops:
// `tab.open` spawns a real PTY, which trips the same swift-testing
// SIGTRAP that disables the PTY paths in LocalClientTests /
// PtySupervisorTests (those stay covered by the manual pass). The
// error-mapping ops here reach the supervisor only on the lookup-
// fails path (no forkpty), so they're safe.

import Foundation
import Testing

@testable import Roost

/// Build a handler over a fresh workspace on `socket`. Returns the
/// workspace too, so a suite that needs a tab can add one without
/// respawning the stack (`tab.open` would fork a real PTY).
@MainActor
private func makeTestHandler(socket: String) -> (IPCHandlerImpl, Workspace) {
    let workspace = Workspace()
    let supervisor = PtySupervisor()
    let client = LocalClient(workspace: workspace, supervisor: supervisor, socketPath: socket)
    let handler = IPCHandlerImpl(
        client: client,
        socketPath: socket,
        appLabel: "Roost-test",
        appID: "ai.stridelabs.Roost.test"
    )
    return (handler, workspace)
}

/// Assert that `handle` throws an `IPCHandlerError` with `code`.
private func expectError(
    _ code: String,
    _ op: String,
    _ params: AnyCodable?,
    on handler: IPCHandlerImpl
) async {
    do {
        _ = try await handler.handle(op: op, params: params)
        Issue.record("expected \(op) to throw \(code)")
    } catch let e as IPCHandlerError {
        #expect(e.code == code, "expected code \(code), got \(e.code): \(e.message)")
    } catch {
        Issue.record("expected IPCHandlerError, got \(error)")
    }
}

@Suite("IPC handler dispatch")
struct IPCHandlerDispatchTests {
    private let socket = "/tmp/roost-ipc-handler-test.sock"

    @MainActor
    private func makeHandler() -> IPCHandlerImpl {
        makeTestHandler(socket: socket).0
    }

    // MARK: cross-cutting error paths

    @Test func eventsSubscribeReturnsNotImplemented() async {
        let handler = await makeHandler()
        await expectError("not-implemented", "events.subscribe", nil, on: handler)
    }

    @Test func unknownOpRejected() async {
        let handler = await makeHandler()
        await expectError("unknown-op", "not.a.real.op", nil, on: handler)
    }

    @Test func unknownParamFieldRejected() async {
        // decodeParams mirrors the Rust deny_unknown_fields policy.
        let handler = await makeHandler()
        await expectError(
            "unknown-field",
            "project.create",
            AnyCodable(["name": "x", "cwd": "/", "bogus": 1] as [String: Any]),
            on: handler
        )
    }

    /// The gated test-only ops MUST refuse without
    /// `ROOST_TEST_MODE=1` at launch — surface a deterministic
    /// `not-enabled` error rather than silently returning empty
    /// data. Unit-test target boots `RoostBackend.shared` without
    /// the env var, so `testMode` is false here.
    @Test func feedPtyBytesRequiresTestMode() async {
        let handler = await makeHandler()
        await expectError(
            "not-enabled",
            "tab.feed_pty_bytes",
            AnyCodable(["tab_id": "1", "data": ""] as [String: Any]),
            on: handler
        )
    }

    @Test func capturePtyInputRequiresTestMode() async {
        let handler = await makeHandler()
        await expectError(
            "not-enabled",
            "tab.capture_pty_input",
            AnyCodable(["tab_id": "1", "drain": true] as [String: Any]),
            on: handler
        )
    }

    @Test func notificationStatusRequiresTestMode() async {
        let handler = await makeHandler()
        await expectError("not-enabled", "app.notification_status", nil, on: handler)
    }

    @Test func sidebarSetWidthRequiresTestMode() async {
        let handler = await makeHandler()
        await expectError(
            "not-enabled",
            "sidebar.set_width",
            AnyCodable(["width": 260.0] as [String: Any]),
            on: handler
        )
    }

    @Test func renameMissingProjectIsNotFound() async {
        // mapWorkspace(.projectNotFound) → not-found.
        let handler = await makeHandler()
        await expectError(
            "not-found",
            "project.rename",
            AnyCodable(["project_id": "999999", "name": "x"] as [String: Any]),
            on: handler
        )
    }

    @Test func resizeColsOutOfRangeIsInvalidParam() async {
        // ipcDim rejects > UInt16.max before touching the supervisor.
        let handler = await makeHandler()
        await expectError(
            "invalid-param",
            "tab.resize",
            AnyCodable(["tab_id": "1", "cols": 70000, "rows": 24] as [String: Any]),
            on: handler
        )
    }

    @Test func resizeMissingTabIsNotFound() async {
        // mapPty(.notFound) → not-found. resize only looks the tab up;
        // it never spawns, so this is SIGTRAP-safe.
        let handler = await makeHandler()
        await expectError(
            "not-found",
            "tab.resize",
            AnyCodable(["tab_id": "999999", "cols": 80, "rows": 24] as [String: Any]),
            on: handler
        )
    }

    // MARK: happy-path encode/decode

    @Test func identifyEchoesProfile() async throws {
        let handler = await makeHandler()
        let result = try await handler.handle(op: "identify", params: nil)
        let dict = result?.value as? [String: Any]
        #expect(dict?["app_label"] as? String == "Roost-test")
        #expect(dict?["app_id"] as? String == "ai.stridelabs.Roost.test")
        #expect((dict?["protocol_version"] as? NSNumber)?.intValue == Int(ipcProtocolVersion))
        #expect(dict?["socket_path"] as? String == socket)
    }

    @Test func projectCreateThenListRoundTrips() async throws {
        let handler = await makeHandler()
        let created = try await handler.handle(
            op: "project.create",
            params: AnyCodable(["name": "proj", "cwd": "/tmp"] as [String: Any])
        )
        let project = (created?.value as? [String: Any])?["project"] as? [String: Any]
        #expect(project?["name"] as? String == "proj")
        #expect((project?["position"] as? NSNumber)?.intValue == 0)

        let listed = try await handler.handle(op: "tab.list", params: nil)
        let projects = (listed?.value as? [String: Any])?["projects"] as? [[String: Any]]
        #expect(projects?.count == 1)
        #expect(projects?.first?["name"] as? String == "proj")
        // A freshly created project has no tabs; this also asserts the
        // `tabs` key encodes as a (here empty) array.
        #expect((projects?.first?["tabs"] as? [[String: Any]])?.isEmpty == true)
    }
}

// `tab.agent_report` (plan 002 §3.6) + the `Tab` wire fields it moves.
@Suite("IPC agent report dispatch")
struct IPCAgentReportDispatchTests {
    private let socket = "/tmp/roost-ipc-agent-report-test.sock"

    /// A handler over a workspace with one tab. The tab is opened
    /// straight on the workspace rather than through `tab.open`, which
    /// would spawn a real PTY (the SIGTRAP the other suites avoid).
    @MainActor
    private func makeHandlerWithTab() throws -> (IPCHandlerImpl, Int64) {
        let (handler, workspace) = makeTestHandler(socket: socket)
        let project = workspace.createProject(name: "p", cwd: "")
        let tab = try workspace.openTab(projectID: project.id, cwd: "/", title: "")
        // Unfocused so attention isn't dropped by policy §3.5 — that
        // matrix is covered in WorkspaceStateTests.
        workspace.setWindowFocused(false)
        return (handler, tab.id)
    }

    private func expectReportError(
        _ code: String,
        _ params: [String: Any],
        on handler: IPCHandlerImpl
    ) async {
        await expectError(code, "tab.agent_report", AnyCodable(params), on: handler)
    }

    @Test func agentReportClaimsAndReturnsTheDerivedTab() async throws {
        let (handler, tabID) = try await makeHandlerWithTab()
        let result = try await handler.handle(
            op: "tab.agent_report",
            params: AnyCodable([
                "tab_id": String(tabID),
                "source": "claude",
                "session_id": "abc123",
                "ownership_action": "claim",
                "lifecycle": "waiting",
                "attention": "set",
                "severity": "warn",
                "title": "Claude Code",
                "body": "Needs your permission",
                "detail": "permission_prompt",
                "metadata": ["model": "claude-opus-5"],
            ] as [String: Any])
        )
        let dict = result?.value as? [String: Any]
        #expect(dict?["accepted"] as? Bool == true)
        let tab = dict?["tab"] as? [String: Any]
        // `state` + `hook_active` are the derived projections.
        #expect(tab?["state"] as? String == "needs_input")
        #expect(tab?["hook_active"] as? Bool == true)
        #expect(tab?["agent_lifecycle"] as? String == "waiting")
        #expect(tab?["shell_state"] as? String == "unknown")
        #expect(tab?["has_notification"] as? Bool == true)
        let ownership = tab?["ownership"] as? [String: Any]
        #expect(ownership?["source"] as? String == "claude")
        #expect(ownership?["session_id"] as? String == "abc123")
        #expect(ownership?["detail"] as? String == "permission_prompt")
    }

    /// A report from a foreign session is a successful op with
    /// `accepted: false` — not an error.
    @Test func agentReportFromAForeignSessionIsAcceptedFalse() async throws {
        let (handler, tabID) = try await makeHandlerWithTab()
        _ = try await handler.handle(
            op: "tab.agent_report",
            params: AnyCodable([
                "tab_id": String(tabID), "source": "claude", "session_id": "s1",
                "ownership_action": "claim", "lifecycle": "working",
            ] as [String: Any])
        )
        let result = try await handler.handle(
            op: "tab.agent_report",
            params: AnyCodable([
                "tab_id": String(tabID), "source": "claude", "session_id": "s2",
                "ownership_action": "preserve", "lifecycle": "finished",
            ] as [String: Any])
        )
        let dict = result?.value as? [String: Any]
        #expect(dict?["accepted"] as? Bool == false)
        #expect((dict?["tab"] as? [String: Any])?["agent_lifecycle"] as? String == "working")
    }

    @Test func agentReportRejectsUnknownField() async throws {
        let (handler, tabID) = try await makeHandlerWithTab()
        await expectReportError(
            "unknown-field",
            [
                "tab_id": String(tabID), "source": "claude",
                "ownership_action": "claim", "last_event_at": 5,
            ],
            on: handler
        )
    }

    @Test func agentReportSetWithoutTitleIsInvalidParam() async throws {
        let (handler, tabID) = try await makeHandlerWithTab()
        await expectReportError(
            "invalid-param",
            [
                "tab_id": String(tabID), "source": "claude",
                "ownership_action": "claim", "attention": "set", "body": "no title",
            ],
            on: handler
        )
    }

    @Test func agentReportOnAMissingTabIsNotFound() async throws {
        let (handler, _) = try await makeHandlerWithTab()
        await expectReportError(
            "not-found",
            ["tab_id": "999999", "source": "claude", "ownership_action": "claim"],
            on: handler
        )
    }

    /// AC 11: a `failed` tab must still decode on a client that only
    /// knows the four legacy states. `IPCTabState` is that closed enum,
    /// so decoding the emitted payload is the guard.
    @Test func failedLifecycleProjectsOntoTheLegacyStateEnum() async throws {
        let (handler, tabID) = try await makeHandlerWithTab()
        let result = try await handler.handle(
            op: "tab.agent_report",
            params: AnyCodable([
                "tab_id": String(tabID), "source": "claude", "session_id": "s1",
                "ownership_action": "claim", "lifecycle": "failed",
            ] as [String: Any])
        )
        let raw = (result?.value as? [String: Any])?["tab"] as? [String: Any]
        let encoded = try JSONSerialization.data(withJSONObject: raw ?? [:])
        let decoded = try JSONDecoder().decode(IPCTab.self, from: encoded)
        #expect(decoded.state == .needsInput)
        #expect(decoded.agentLifecycle == .failed)
    }

    /// The agent axes are additive: a `Tab` encoded by a server
    /// predating plan 002 still decodes, with every axis on its default.
    /// `IPCTab`'s decoder is strict (`try c.decode`) everywhere else, so
    /// this is the field-by-field guard that the new keys used
    /// `decodeIfPresent`.
    @Test func tabDecodesWithoutTheAgentAxes() throws {
        let legacy = """
        {"id":"5","project_id":"1","title":"zsh","cwd":"/tmp","state":"running",
         "has_notification":false,"is_active":true,"user_titled":false,"position":0,
         "created_at":1,"last_active":2,"hook_active":false}
        """
        let tab = try JSONDecoder().decode(IPCTab.self, from: Data(legacy.utf8))
        #expect(tab.shellState == .unknown)
        #expect(tab.agentLifecycle == .inactive)
        #expect(tab.ownership == nil)
    }
}

/// `IPCPaletteItemView.agent` is additive (plan 005 §3.9): absent on
/// every non-agent row, present only on rows the (not-yet-built) agents
/// frame produces. These decode/re-encode fixtures pin both shapes so a
/// drift in `IPCPaletteAgentRow`'s `CodingKeys` or the omit-when-nil
/// behavior surfaces here rather than in the agents-frame commit.
/// Byte-parity with Rust's `{:?}` in the invalid-kind message
/// (`crates/roost-engine/src/ipc.rs`, shared by iced) — quotes and
/// escapes must not diverge between the two UIs.
@Suite("Rust {:?} string parity")
struct RustDebugQuotedTests {
    @Test func matchesRustDebugFormatting() {
        #expect(IPCHandlerImpl.rustDebugQuoted("bogus") == "\"bogus\"")
        #expect(IPCHandlerImpl.rustDebugQuoted("n\0l") == "\"n\\0l\"")
        #expect(IPCHandlerImpl.rustDebugQuoted("x\"y") == "\"x\\\"y\"")
        #expect(IPCHandlerImpl.rustDebugQuoted("a\\b") == "\"a\\\\b\"")
        #expect(IPCHandlerImpl.rustDebugQuoted("t\ta") == "\"t\\ta\"")
        #expect(IPCHandlerImpl.rustDebugQuoted("n\nr\r") == "\"n\\nr\\r\"")
        #expect(IPCHandlerImpl.rustDebugQuoted("bel\u{7}") == "\"bel\\u{7}\"")
    }
}

@Suite("IPC palette item view — agent payload")
struct IPCPaletteItemViewTests {
    @Test func decodesWithoutAnAgentPayload() throws {
        let json = """
        {"id":"new_tab","title":"New Tab"}
        """
        let item = try JSONDecoder().decode(IPCPaletteItemView.self, from: Data(json.utf8))
        #expect(item.subtitle == nil)
        #expect(item.agent == nil)

        let encoded = try JSONEncoder().encode(item)
        let obj = try JSONSerialization.jsonObject(with: encoded) as? [String: Any]
        #expect(obj?["agent"] == nil)
    }

    @Test func decodesAndReencodesAFullAgentPayload() throws {
        let json = """
        {"id":"agent:3","title":"roost · slauth-refactor",
         "agent":{"effective_lifecycle":"waiting","project":"roost",
                  "name":"slauth-refactor","status_text":"Waiting for input",
                  "time_text":"2m","metrics_text":"4f +86 -12"}}
        """
        let item = try JSONDecoder().decode(IPCPaletteItemView.self, from: Data(json.utf8))
        let agent = try #require(item.agent)
        #expect(agent.effectiveLifecycle == .waiting)
        #expect(agent.project == "roost")
        #expect(agent.name == "slauth-refactor")
        #expect(agent.statusText == "Waiting for input")
        #expect(agent.timeText == "2m")
        #expect(agent.metricsText == "4f +86 -12")

        let encoded = try JSONEncoder().encode(item)
        let obj = try JSONSerialization.jsonObject(with: encoded) as? [String: Any]
        let reencodedAgent = obj?["agent"] as? [String: Any]
        #expect(reencodedAgent?["metrics_text"] as? String == "4f +86 -12")
    }

    /// A malformed `agent` payload (possible on caller-supplied
    /// `palette.present` items, documented as ignored) must decode to
    /// nil, not fail the whole request — mirrors the Rust side's
    /// lenient deserializer.
    @Test func malformedAgentPayloadDecodesToNil() throws {
        for junk in [
            #"{"id":"x","title":"t","agent":"garbage"}"#,
            #"{"id":"x","title":"t","agent":{"effective_lifecycle":"no-such"}}"#,
            #"{"id":"x","title":"t","agent":7}"#,
        ] {
            let item = try JSONDecoder().decode(IPCPaletteItemView.self, from: Data(junk.utf8))
            #expect(item.agent == nil)
            #expect(item.id == "x")
        }
    }

    /// `metrics_text` absent (pending probe) is a distinct, observable
    /// wire shape from `metrics_text` present — the agents frame relies
    /// on this to show a row before its git metrics resolve.
    @Test func pendingMetricsOmitsTheKeyOnReencode() throws {
        let json = """
        {"id":"agent:4","title":"roost · pending-metrics",
         "agent":{"effective_lifecycle":"working","project":"roost",
                  "name":"pending-metrics","status_text":"Working","time_text":"41s"}}
        """
        let item = try JSONDecoder().decode(IPCPaletteItemView.self, from: Data(json.utf8))
        #expect(item.agent?.metricsText == nil)

        let encoded = try JSONEncoder().encode(item)
        let obj = try JSONSerialization.jsonObject(with: encoded) as? [String: Any]
        let reencodedAgent = obj?["agent"] as? [String: Any]
        #expect(reencodedAgent != nil)
        #expect(reencodedAgent?["metrics_text"] == nil)
    }
}

/// `app.sidebar_dump`'s Codable mirror (plan 007 §3.8, A8). Generic
/// golden-vector round-tripping only proves the fixture is valid JSON;
/// this decodes into the typed struct so a field-name/string-id
/// mismatch against the Rust side (`SidebarDumpResult` in
/// `crates/roost-ipc/src/messages.rs`) fails here, matching the
/// pinned wire example in the plan.
struct IPCSidebarDumpResultTests {
    @Test func decodesStringIdsAndAllProjectsIncludingEmptyOnes() throws {
        let json = """
        {"agents_visible":true,
         "projects":[{"project_id":"1",
                       "agents":[{"tab_id":"7","name":"slauth-refactor",
                                  "lifecycle":"waiting","status_text":"Waiting for input",
                                  "time_text":"2m","is_active":false}]},
                      {"project_id":"2","agents":[]}]}
        """
        let result = try JSONDecoder().decode(IPCSidebarDumpResult.self, from: Data(json.utf8))
        #expect(result.agentsVisible)
        #expect(result.projects.count == 2)
        #expect(result.projects[0].projectID == 1)
        #expect(result.projects[0].agents.count == 1)
        let row = try #require(result.projects[0].agents.first)
        #expect(row.tabID == 7)
        #expect(row.name == "slauth-refactor")
        #expect(row.lifecycle == .waiting)
        #expect(row.statusText == "Waiting for input")
        #expect(row.timeText == "2m")
        #expect(!row.isActive)
        #expect(result.projects[1].projectID == 2)
        #expect(result.projects[1].agents.isEmpty)

        let encoded = try JSONEncoder().encode(result)
        let obj = try JSONSerialization.jsonObject(with: encoded) as? [String: Any]
        let projects = obj?["projects"] as? [[String: Any]]
        #expect(projects?[0]["project_id"] as? String == "1")
        let agents = projects?[0]["agents"] as? [[String: Any]]
        #expect(agents?[0]["tab_id"] as? String == "7")
        #expect(projects?[1]["project_id"] as? String == "2")
        #expect((projects?[1]["agents"] as? [[String: Any]])?.isEmpty == true)
    }
}

/// `app.notification_status`'s Codable mirror. The op exists so one
/// test asserts identically against both UIs, so the encoding has to
/// match `AppNotificationStatusResult` in
/// `crates/roost-ipc/src/messages.rs` — including the `reason` key,
/// which the Rust struct carries no `skip_serializing_if` for and which
/// Swift's synthesized `encodeIfPresent` would otherwise drop when nil.
struct IPCAppNotificationStatusResultTests {
    @Test func decodesIcedsShapeAndKeepsAnExplicitNullReason() throws {
        let json = """
        {"backend":"available","reason":null,"authorized":true}
        """
        let result = try JSONDecoder().decode(
            IPCAppNotificationStatusResult.self, from: Data(json.utf8)
        )
        #expect(result.backend == "available")
        #expect(result.reason == nil)
        #expect(result.authorized)

        let obj = try JSONSerialization.jsonObject(
            with: JSONEncoder().encode(result)
        ) as? [String: Any]
        #expect(obj?["backend"] as? String == "available")
        #expect(obj?["authorized"] as? Bool == true)
        #expect(obj?["reason"] is NSNull, "reason stays on the wire as null, not omitted")
    }

    @Test func carriesTheUnavailableReasonThrough() throws {
        let value = IPCAppNotificationStatusResult(
            backend: "unavailable", reason: "no app bundle", authorized: false
        )
        let obj = try JSONSerialization.jsonObject(
            with: JSONEncoder().encode(value)
        ) as? [String: Any]
        #expect(obj?["backend"] as? String == "unavailable")
        #expect(obj?["reason"] as? String == "no app bundle")
        #expect(obj?["authorized"] as? Bool == false)
    }
}
