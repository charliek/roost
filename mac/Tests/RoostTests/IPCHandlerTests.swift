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

@Suite("IPC handler dispatch")
struct IPCHandlerDispatchTests {
    private let socket = "/tmp/roost-ipc-handler-test.sock"

    @MainActor
    private func makeHandler() -> IPCHandlerImpl {
        let workspace = Workspace()
        let supervisor = PtySupervisor()
        let client = LocalClient(workspace: workspace, supervisor: supervisor, socketPath: socket)
        return IPCHandlerImpl(
            client: client,
            socketPath: socket,
            appLabel: "Roost-test",
            appID: "ai.stridelabs.Roost.test"
        )
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
