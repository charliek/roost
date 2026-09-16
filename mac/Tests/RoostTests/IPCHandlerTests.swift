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

    // MARK: #402 — non-canonical ids are refused, not normalized
    //
    // `StringInt64`/`StringInt64Array` round-trip-check the decoded
    // int64 against the original text (`String(v) == raw`), mirroring
    // the Rust `WireTabRef::parse`/`WireProjectRef::parse` narrowing.
    // Swift's `Int64("+4")` and `Int64("04")` both succeed, so without
    // the round-trip check these would silently normalize to `4`
    // instead of failing `invalid-param` like the iced/session sockets.
    // The failure surfaces as `invalid-param` because `decodeParams`
    // wraps every JSONDecoder error the same way, regardless of which
    // wrapper threw it.

    @Test func tabReorderNonCanonicalProjectIdWithLeadingPlusIsInvalidParam() async {
        let handler = await makeHandler()
        await expectError(
            "invalid-param",
            "tab.reorder",
            AnyCodable(["project_id": "+4", "tab_ids": ["1"]] as [String: Any]),
            on: handler
        )
    }

    @Test func tabReorderNonCanonicalTabIdWithLeadingZeroIsInvalidParam() async {
        let handler = await makeHandler()
        await expectError(
            "invalid-param",
            "tab.reorder",
            AnyCodable(["project_id": "1", "tab_ids": ["04"]] as [String: Any]),
            on: handler
        )
    }

    @Test func projectReorderNonCanonicalIdWithLeadingPlusIsInvalidParam() async {
        let handler = await makeHandler()
        await expectError(
            "invalid-param",
            "project.reorder",
            AnyCodable(["project_ids": ["+4"]] as [String: Any]),
            on: handler
        )
    }

    @Test func projectReorderNonCanonicalIdWithLeadingZeroIsInvalidParam() async {
        let handler = await makeHandler()
        await expectError(
            "invalid-param",
            "project.reorder",
            AnyCodable(["project_ids": ["04"]] as [String: Any]),
            on: handler
        )
    }

    // The plan 065 discovery record that added the tests above named
    // only `tab.reorder`/`project.reorder` as `WireTabRef`-backed.
    // `tab.dump` and `tab.dump_resolved` are too (Rust `messages.rs`'s
    // `TabDumpParams`/`TabDumpResolvedParams`) and used a bare
    // `Int64(raw)` decode until now — same bug, same fix
    // (`decodeCanonicalStringInt64` in IPCHandlerImpl.swift). Both
    // decode params before any tab lookup, so no tab needs to exist
    // for these. `tab.capture_pty_input` is the third `WireTabRef`
    // struct still fixed here, but it can't be unit-tested this way:
    // it checks `RoostBackend.shared.testMode` (false in this test
    // binary — see `capturePtyInputRequiresTestMode` above) and
    // throws `not-enabled` before decode ever runs. That op's
    // canonical-id coverage lives in
    // `tools/roosttest/test_test_ops.py::test_capture_pty_input_refuses_a_non_canonical_id`
    // instead, run with `ROOST_TEST_MODE=1` against a real socket.

    @Test func tabDumpNonCanonicalIdWithLeadingPlusIsInvalidParam() async {
        let handler = await makeHandler()
        await expectError(
            "invalid-param",
            "tab.dump",
            AnyCodable(["tab_id": "+4"] as [String: Any]),
            on: handler
        )
    }

    @Test func tabDumpNonCanonicalIdWithLeadingZeroIsInvalidParam() async {
        let handler = await makeHandler()
        await expectError(
            "invalid-param",
            "tab.dump",
            AnyCodable(["tab_id": "04"] as [String: Any]),
            on: handler
        )
    }

    @Test func tabDumpResolvedNonCanonicalIdWithLeadingPlusIsInvalidParam() async {
        let handler = await makeHandler()
        await expectError(
            "invalid-param",
            "tab.dump_resolved",
            AnyCodable(["tab_id": "+4"] as [String: Any]),
            on: handler
        )
    }

    @Test func tabDumpResolvedNonCanonicalIdWithLeadingZeroIsInvalidParam() async {
        let handler = await makeHandler()
        await expectError(
            "invalid-param",
            "tab.dump_resolved",
            AnyCodable(["tab_id": "04"] as [String: Any]),
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
        // `makeHandler`'s workspace is in-memory (no statePath), so
        // persist() never runs and `persistError` stays nil — the key
        // must be absent from the wire entirely, not present as null
        // (#481; mirrors Rust's `skip_serializing_if`).
        #expect(dict?.keys.contains("persist_error") == false)
    }

    // #481: `IPCIdentifyResult.persistError` mirrors Rust's
    // `IdentifyResult.persist_error` — `encodeIfPresent` must drop the
    // key entirely when there's no error (not encode it as `null`),
    // so `identify.response.json` still decodes unchanged, and must
    // put the exact error text on the wire when there is one.
    @Test func identifyResultOmitsPersistErrorWhenNilButIncludesItWhenSet() throws {
        let clean = IPCIdentifyResult(
            socketPath: "/tmp/x.sock", pid: 1,
            activeProjectID: 1, activeTabID: 1,
            appLabel: "Roost", appID: "ai.stridelabs.Roost",
            uiVersion: "0.0.0", protocolVersion: 1
        )
        let cleanJSON = String(decoding: try JSONEncoder().encode(clean), as: UTF8.self)
        #expect(!cleanJSON.contains("persist_error"), "absent, not null, when there is no error")

        var failing = clean
        failing.persistError = "Read-only file system (os error 30)"
        let failingData = try JSONEncoder().encode(failing)
        let failingJSON = String(decoding: failingData, as: UTF8.self)
        #expect(failingJSON.contains(#""persist_error":"Read-only file system (os error 30)""#))

        let decoded = try JSONDecoder().decode(IPCIdentifyResult.self, from: failingData)
        #expect(decoded.persistError == failing.persistError)
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

    /// `lifecycle_if` (plan 046 §3.8) end to end through the dispatcher.
    /// Two failures this catches, and they look identical from the
    /// outside: a `CodingKeys` case that never got added (the
    /// `decodeParams(expected:)` set is derived from it, so the whole
    /// report would come back `unknown-field`), and a decoder that
    /// accepts the key while `applyReport` ignores it — which would
    /// apply the patch and banner a turn that already ended.
    @Test func agentReportGuardVetoesTheLifecyclePatchAndItsAttention() async throws {
        let (handler, tabID) = try await makeHandlerWithTab()
        _ = try await handler.handle(
            op: "tab.agent_report",
            params: AnyCodable([
                "tab_id": String(tabID), "source": "claude", "session_id": "s1",
                "ownership_action": "claim", "lifecycle": "finished",
            ] as [String: Any])
        )

        let result = try await handler.handle(
            op: "tab.agent_report",
            params: AnyCodable([
                "tab_id": String(tabID), "source": "claude", "session_id": "s1",
                "ownership_action": "preserve",
                "lifecycle": "waiting",
                "lifecycle_if": ["working"],
                "attention": "set",
                "severity": "warn",
                "title": "Claude Code",
                "body": "Claude is waiting for your input",
                "detail": "idle_prompt",
            ] as [String: Any])
        )
        let dict = result?.value as? [String: Any]
        // Ownership matched, so the report is accepted — only the patch
        // and its notification were dropped.
        #expect(dict?["accepted"] as? Bool == true)
        let tab = dict?["tab"] as? [String: Any]
        #expect(tab?["agent_lifecycle"] as? String == "finished")
        #expect(tab?["state"] as? String == "idle")
        #expect(tab?["has_notification"] as? Bool == false)
        // `detail` is unguarded, so it still merges — the proof the
        // report was applied rather than dropped whole.
        let ownership = tab?["ownership"] as? [String: Any]
        #expect(ownership?["detail"] as? String == "idle_prompt")
    }

    /// The other half of the guard: in the set, the patch applies
    /// normally. Without this a `lifecycleIf` that vetoed everything
    /// would pass the test above.
    @Test func agentReportGuardAppliesThePatchWhenTheLifecycleMatches() async throws {
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
                "tab_id": String(tabID), "source": "claude", "session_id": "s1",
                "ownership_action": "preserve",
                "lifecycle": "finished",
                "lifecycle_if": ["working"],
                "attention": "set",
                "severity": "info",
                "title": "Claude Code",
                "body": "Turn complete",
            ] as [String: Any])
        )
        let tab = (result?.value as? [String: Any])?["tab"] as? [String: Any]
        #expect(tab?["agent_lifecycle"] as? String == "finished")
        #expect(tab?["has_notification"] as? Bool == true)
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
        {"id":"agent:3","title":"Claude Code · roost · slauth-refactor",
         "agent":{"effective_lifecycle":"waiting","agent":"Claude Code","project":"roost",
                  "name":"slauth-refactor","status_text":"Waiting for input",
                  "time_text":"2m","metrics_text":"4f +86 -12"}}
        """
        let item = try JSONDecoder().decode(IPCPaletteItemView.self, from: Data(json.utf8))
        let agent = try #require(item.agent)
        #expect(agent.effectiveLifecycle == .waiting)
        #expect(agent.agent == "Claude Code")
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
        {"id":"agent:4","title":"Claude Code · roost · pending-metrics",
         "agent":{"effective_lifecycle":"working","agent":"Claude Code","project":"roost",
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
        // This UI has no band strip, and an absent one must stay off the
        // wire so the reply is byte-identical to the pre-063 shape.
        #expect(result.sections == nil)
        #expect(obj?["sections"] == nil)
    }

    /// The band strip (plan 063 §D2) is iced-only, but the mirror still
    /// has to decode one: a Swift client reading an iced UI's dump is
    /// exactly the direction the shared vector corpus guards.
    @Test func decodesTheBandStripAnIcedUIEmitsUnderSession() throws {
        let json = """
        {"agents_visible":true,"projects":[],
         "sections":[{"role":"session","label":"PROJECTS","state":"connected",
                      "dot":"connected","saved_id":"hs-2f1c","reconnect_row":false},
                     {"role":"host","label":"WORKBENCH","state":"disconnected",
                      "dot":"offline","saved_id":"hs-9d40","reconnect_row":true,
                      "fidelity":"update"}]}
        """
        let result = try JSONDecoder().decode(IPCSidebarDumpResult.self, from: Data(json.utf8))
        let sections = try #require(result.sections)
        #expect(sections.count == 2)
        #expect(sections[0].role == "session")
        #expect(sections[0].label == "PROJECTS")
        #expect(sections[0].dot == "connected")
        #expect(sections[0].savedID == "hs-2f1c")
        #expect(!sections[0].reconnectRow)
        #expect(sections[0].fidelity == nil)
        #expect(sections[1].role == "host")
        #expect(sections[1].savedID == "hs-9d40")
        #expect(sections[1].reconnectRow)
        #expect(sections[1].fidelity == "update")
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

/// `agent.set_hooks` — the Mac's half of plan 064 §3.4.
///
/// The op spawns exactly one `roostctl agent set --local … --json` and
/// answers from its JSON. The handler's runner is injected, so these
/// never reach a real `roostctl` or a real dotfile.
@Suite("IPC handler: agent.set_hooks")
struct IPCHandlerAgentSetHooksTests {
    private let socket = "/tmp/roost-ipc-agent-set-hooks-test.sock"

    @MainActor
    private func makeHandler(_ log: AgentHooksSpawnLog) -> IPCHandlerImpl {
        let workspace = Workspace()
        let supervisor = PtySupervisor()
        let client = LocalClient(
            workspace: workspace, supervisor: supervisor, socketPath: socket)
        return IPCHandlerImpl(
            client: client,
            socketPath: socket,
            appLabel: "Roost-test",
            appID: "ai.stridelabs.Roost.test",
            agentHooks: log.runner
        )
    }

    /// One writer, one spawn — and the two variables that decide which
    /// `config.conf` the child writes go with it.
    @Test func aListSpawnsOneLocalSetAndAnswersFromItsJSON() async throws {
        let log = AgentHooksSpawnLog(
            roostctl: "/bin/roostctl",
            reply: { _ in
                AgentHooksRun(
                    status: 0,
                    stdout: """
                        {"wired":["claude"],"refreshed":["codex"],"current":[],"removed":[],\
                        "skipped":[{"agent":"grok","reason":"not installed"}],\
                        "warnings":[],"errors":[]}
                        """)
            })
        let handler = await makeHandler(log)

        let result = try await handler.handle(
            op: "agent.set_hooks",
            params: AnyCodable(["agents": ["claude", "codex"]])
        )

        let spawned = log.commands()
        #expect(spawned.count == 1, "expected exactly one spawn, got \(spawned.count)")
        #expect(
            spawned.first?.argv == [
                "/bin/roostctl", "agent", "set", "--local", "claude,codex", "--json",
            ])
        #expect(spawned.first?.environment["HOME"] == "/home/test-u")
        #expect(spawned.first?.environment["ROOST_CONFIG"] == "/tmp/roost-test/config.conf")

        let body = result?.value as? [String: Any]
        let local = body?["local"] as? [String: Any]
        #expect(local?["wired"] as? [String] == ["claude"])
        #expect(local?["refreshed"] as? [String] == ["codex"])
        #expect((local?["skipped"] as? [[String: Any]])?.first?["agent"] as? String == "grok")
        #expect((body?["config_path"] as? String)?.isEmpty == false)
        // The Mac holds no host connections, so the raise list is always
        // empty — the field is on the reply because the shape is shared.
        #expect((body?["hosts"] as? [Any])?.isEmpty == true)
    }

    /// The spec is the inventory's order, not the caller's: the same
    /// answer however a client spells the list, and the same spec the
    /// iced UI resolves that list to. Case and duplicates normalise
    /// along the way, as they do on the Rust side.
    @Test func theSpecIsInInventoryOrderWhateverOrderTheCallerSent() async throws {
        let log = AgentHooksSpawnLog(
            roostctl: "/bin/roostctl",
            reply: { _ in
                AgentHooksRun(
                    status: 0,
                    stdout: """
                        {"wired":[],"refreshed":[],"current":[],"removed":[],\
                        "skipped":[],"warnings":[],"errors":[]}
                        """)
            })
        let handler = await makeHandler(log)

        _ = try await handler.handle(
            op: "agent.set_hooks",
            params: AnyCodable(["agents": ["opencode", "CLAUDE", "codex", "claude"]])
        )

        #expect(
            log.commands().first?.argv == [
                "/bin/roostctl", "agent", "set", "--local", "claude,codex,opencode", "--json",
            ])
    }

    @Test func offTravelsAsTheWordAndNotAnEmptyList() async throws {
        let log = AgentHooksSpawnLog(
            roostctl: "/bin/roostctl",
            reply: { _ in
                AgentHooksRun(
                    status: 0,
                    stdout: """
                        {"wired":[],"refreshed":[],"current":[],"removed":["claude"],\
                        "skipped":[],"warnings":[],"errors":[]}
                        """)
            })
        let handler = await makeHandler(log)

        let result = try await handler.handle(
            op: "agent.set_hooks", params: AnyCodable(["agents": "off"]))

        #expect(
            log.commands().first?.argv == [
                "/bin/roostctl", "agent", "set", "--local", "off", "--json",
            ])
        let local = (result?.value as? [String: Any])?["local"] as? [String: Any]
        #expect(local?["removed"] as? [String] == ["claude"])
    }

    /// Refused before anything is written, and matching the iced side's
    /// rule exactly: a consent answer has no honest partial reading.
    @Test func anEmptyListAndAnUnknownNameAreBothRefusedWithoutSpawning() async {
        let log = AgentHooksSpawnLog(roostctl: "/bin/roostctl")
        let handler = await makeHandler(log)

        for agents in [[String](), ["claude", "gemini"], [""]] {
            await expectError(
                "invalid-param", "agent.set_hooks", AnyCodable(["agents": agents]), on: handler)
        }
        // A string that is not the one word this field means is a decode
        // failure, not a silent `off`.
        await expectError(
            "invalid-param", "agent.set_hooks", AnyCodable(["agents": "none"]), on: handler)
        #expect(log.commands().isEmpty, "a refused request still reached roostctl")
    }

    /// A whole-run failure — no outcome printed at all — is an error
    /// frame; a per-agent failure riding inside a printed outcome is not.
    @Test func aRunThatPrintedNoOutcomeIsAnErrorFrame() async {
        let log = AgentHooksSpawnLog(
            roostctl: "/bin/roostctl",
            reply: { _ in AgentHooksRun(status: 1, stderr: "roostctl agent: no $HOME") })
        let handler = await makeHandler(log)
        await expectError(
            "internal", "agent.set_hooks", AnyCodable(["agents": ["claude"]]), on: handler)
    }

    @Test func aPerAgentFailureRidesInsideASuccessfulReply() async throws {
        let log = AgentHooksSpawnLog(
            roostctl: "/bin/roostctl",
            reply: { _ in
                AgentHooksRun(
                    status: 1,
                    stdout: """
                        {"wired":[],"refreshed":[],"current":[],"removed":[],"skipped":[],\
                        "warnings":[],"errors":[{"agent":"codex","error":"bad config.toml"}]}
                        """)
            })
        let handler = await makeHandler(log)
        let result = try await handler.handle(
            op: "agent.set_hooks", params: AnyCodable(["agents": ["codex"]]))
        let errors =
            ((result?.value as? [String: Any])?["local"] as? [String: Any])?["errors"]
            as? [[String: Any]]
        #expect(errors?.first?["agent"] as? String == "codex")
    }

    @Test func unknownParamFieldsAreStillRejected() async {
        let handler = await makeHandler(AgentHooksSpawnLog(roostctl: "/bin/roostctl"))
        await expectError(
            "unknown-field", "agent.set_hooks",
            AnyCodable(["agents": ["claude"], "client": "roostctl"]), on: handler)
    }
}
