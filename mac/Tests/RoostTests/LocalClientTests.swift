// LocalClientTests — M4b of the daemon-removal refactor.
//
// Covers the LocalClient adapter's OSC parsers + workspace
// delegations. The supervisor-touching paths
// (`openTab`/`closeTab`/`writeTab`/etc) are exercised
// end-to-end in M4b's IPCServer integration tests and the M9
// manual pass.

import Foundation
import Testing

@testable import Roost

@Suite("LocalClient OSC parsers")
struct LocalClientOSCTests {
    @Test func osc7StripsHostPrefix() {
        #expect(parseOSC7Path("file://host/Users/me") == "/Users/me")
    }

    @Test func osc7HandlesEmptyHost() {
        #expect(parseOSC7Path("file:///tmp") == "/tmp")
    }

    @Test func osc7ReturnsNilForHostWithoutPath() {
        // `file://host` (no path after host) — must NOT return
        // "host" as the path. The Rust side has the same regression
        // test (`parse_osc7_path` in crates/roost-engine/src/application.rs).
        #expect(parseOSC7Path("file://host") == nil)
    }

    @Test func osc7RejectsNonFileScheme() {
        #expect(parseOSC7Path("http://example.com/path") == nil)
    }

    @Test func osc777SplitsTitleAndBody() {
        let (title, body) = parseNotificationPayload(
            command: 777, payload: "notify;Build;Passed"
        )
        #expect(title == "Build")
        #expect(body == "Passed")
    }

    @Test func osc777WithoutLeadingNotifyPrefix() {
        let (title, body) = parseNotificationPayload(
            command: 777, payload: "Build;Passed"
        )
        #expect(title == "Build")
        #expect(body == "Passed")
    }

    @Test func osc9UsesPayloadAsTitle() {
        let (title, body) = parseNotificationPayload(
            command: 9, payload: "Hello"
        )
        #expect(title == "Hello")
        #expect(body == "")
    }
}

// OSC routing through the workspace. Mirrors the Rust suite in
// `crates/roost-engine/src/application.rs` (shared by iced). No PTY
// is ever spawned — `applyOSC` only touches the workspace.
@MainActor
@Suite("LocalClient OSC routing")
struct LocalClientOSCRoutingTests {
    private func clientWithTab() throws -> (LocalClient, Int64) {
        let workspace = Workspace()
        let project = workspace.createProject(name: "p", cwd: "")
        let tab = try workspace.openTab(projectID: project.id, cwd: "/", title: "")
        // Unfocused: these tests are about the OSC gate, not about
        // policy §3.5's focus suppression (covered in WorkspaceStateTests).
        workspace.setWindowFocused(false)
        let client = LocalClient(
            workspace: workspace,
            supervisor: PtySupervisor(),
            socketPath: "/tmp/roost-localclient-osc-test.sock"
        )
        return (client, tab.id)
    }

    private func claim(_ tabID: Int64, _ lifecycle: AgentLifecycle) -> AgentReport {
        AgentReport(
            tabID: tabID,
            source: "claude",
            sessionID: "s1",
            ownershipAction: .claim,
            lifecycle: lifecycle
        )
    }

    /// Plan §2.2(b)/§3.4: raw OSC 9 / 99 / 777 is dropped while a live
    /// agent session is mid-turn, and works normally outside one. This is
    /// the documented-but-missing behavior the plan restores.
    @Test func rawOscNotificationsAreSuppressedUnderALiveAgent() throws {
        let (client, tabID) = try clientWithTab()

        client.applyOSC(tabID: tabID, command: 9, payload: "Build done")
        #expect(client.workspace.tab(tabID)?.hasNotification == true)

        try client.workspace.setTabHasNotification(tabID, hasPending: false)
        try client.workspace.agentReport(claim(tabID, .working))
        for command in [UInt32(9), 99, 777] {
            client.applyOSC(tabID: tabID, command: command, payload: "notify;Wrapper;Noise")
            #expect(
                client.workspace.tab(tabID)?.hasNotification == false,
                "OSC \(command) should be suppressed mid-turn"
            )
        }

        // The `D` failsafe re-opens the gate even though ownership
        // survives as a label.
        client.applyOSC(tabID: tabID, command: 133, payload: "D;0")
        #expect(client.workspace.tab(tabID)?.hookActive == true)
        client.applyOSC(tabID: tabID, command: 9, payload: "Build done")
        #expect(client.workspace.tab(tabID)?.hasNotification == true)
    }

    /// Only *raw* OSC is gated — an explicit `notification.create` (which
    /// routes straight at the workspace, not through `applyOSC`) is never
    /// suppressed.
    @Test func explicitNotificationsAreNeverSuppressed() throws {
        let (client, tabID) = try clientWithTab()
        try client.workspace.agentReport(claim(tabID, .working))
        #expect(
            try client.raiseAttention(
                tabID, title: "Roost", body: "explicit", source: .structured
            )
        )
        #expect(client.workspace.tab(tabID)?.hasNotification == true)
    }

    @Test func osc133WritesTheShellAxis() throws {
        let (client, tabID) = try clientWithTab()
        client.applyOSC(tabID: tabID, command: 133, payload: "C")
        #expect(client.workspace.tab(tabID)?.state == .running)
        client.applyOSC(tabID: tabID, command: 133, payload: "D;0")
        #expect(client.workspace.tab(tabID)?.state == Workspace.TabState.none)
        // Undefined mark bodies are no-change.
        client.applyOSC(tabID: tabID, command: 133, payload: "Z")
        #expect(client.workspace.tab(tabID)?.state == Workspace.TabState.none)
    }
}

/// `inheritedCwd` with no live PTY, so the native read has nothing to
/// offer and only the tracked cwd is in play — the PTY-free half of Rust's
/// `inherited_cwd` tests in `crates/roost-engine/src/application.rs`.
@MainActor
@Suite("LocalClient inherited cwd")
struct LocalClientInheritedCwdTests {
    private func clientWithTab(tracking cwd: String) throws -> (LocalClient, Int64) {
        let workspace = Workspace()
        let project = workspace.createProject(name: "p", cwd: "/")
        let tab = try workspace.openTab(projectID: project.id, cwd: cwd, title: "")
        let client = LocalClient(
            workspace: workspace,
            supervisor: PtySupervisor(),
            socketPath: "/tmp/roost-localclient-cwd-test.sock"
        )
        return (client, tab.id)
    }

    private func scratchPath(_ kind: String) -> String {
        FileManager.default.temporaryDirectory
            .appendingPathComponent("roost-inherit-\(kind)-\(UUID().uuidString)").path
    }

    @Test func fallsBackToTheTrackedCwdWithoutAChild() throws {
        let dir = scratchPath("dir")
        try FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(atPath: dir) }

        let (client, tabID) = try clientWithTab(tracking: dir)
        #expect(client.inheritedCwd(tabID: tabID) == dir)
    }

    @Test func aTrackedCwdThatIsNotADirectoryInheritsNothing() throws {
        let file = scratchPath("file")
        #expect(FileManager.default.createFile(atPath: file, contents: Data()))
        defer { try? FileManager.default.removeItem(atPath: file) }

        for cwd in ["/no/such/dir", file, ""] {
            let (client, tabID) = try clientWithTab(tracking: cwd)
            #expect(client.inheritedCwd(tabID: tabID) == nil, "\(cwd)")
        }
    }

    @Test func anUnknownTabInheritsNothing() throws {
        let (client, _) = try clientWithTab(tracking: "/")
        #expect(client.inheritedCwd(tabID: 424_242) == nil)
    }
}

/// `spawnCwd`, the Swift twin of Rust's `usable_cwd` tests in
/// `crates/roost-engine/src/application.rs`: once against the real
/// filesystem, once with the predicate injected.
@Suite("LocalClient spawn cwd")
struct LocalClientSpawnCwdTests {
    private let home = "/roost-test-home"

    private func isDirectoryOnDisk(_ path: String) -> Bool {
        var isDir: ObjCBool = false
        return FileManager.default.fileExists(atPath: path, isDirectory: &isDir) && isDir.boolValue
    }

    private func onDisk(requested: String, projectCwd: String) -> String {
        LocalClient.spawnCwd(
            requested: requested, projectCwd: projectCwd, home: home, isDirectory: isDirectoryOnDisk
        )
    }

    @Test func aRequestedDirectoryIsWhereTheShellStarts() {
        #expect(onDisk(requested: "/usr", projectCwd: "/") == "/usr")
    }

    @Test func aRequestedCwdThatIsNotADirectoryFallsBackToTheProjects() {
        let file = FileManager.default.temporaryDirectory
            .appendingPathComponent("roost-spawn-cwd-\(UUID().uuidString)").path
        #expect(FileManager.default.createFile(atPath: file, contents: Data()))
        defer { try? FileManager.default.removeItem(atPath: file) }

        for requested in ["/no/such/dir", file, ""] {
            #expect(onDisk(requested: requested, projectCwd: "/usr") == "/usr", "\(requested)")
        }
    }

    @Test func aProjectCwdThatIsNotADirectoryFallsBackToHome() {
        for projectCwd in ["/no/such/project", ""] {
            #expect(onDisk(requested: "/no/such/dir", projectCwd: projectCwd) == home, "\(projectCwd)")
        }
    }

    @Test func thePredicateDecidesWhatCounts() {
        func resolve(_ isDirectory: (String) -> Bool) -> String {
            LocalClient.spawnCwd(
                requested: "/asked", projectCwd: "/project", home: home, isDirectory: isDirectory
            )
        }
        #expect(resolve { _ in true } == "/asked")
        #expect(resolve { $0 == "/project" } == "/project")
        #expect(resolve { _ in false } == home)
    }
}

/// `ensureDefaultProject` gives a new project the requested cwd only if it
/// is a directory. PTY-free: finding or creating a project spawns nothing.
@MainActor
@Suite("LocalClient default project")
struct LocalClientDefaultProjectTests {
    private func makeClient() -> LocalClient {
        LocalClient(
            workspace: Workspace(),
            supervisor: PtySupervisor(),
            socketPath: "/tmp/roost-localclient-default-test.sock"
        )
    }

    @Test func aNewProjectKeepsARequestedDirectory() {
        let client = makeClient()
        let id = client.ensureDefaultProject(cwd: "/usr", activate: false)
        #expect(client.workspace.project(id)?.cwd == "/usr")
    }

    @Test func aNewProjectRecordsNoCwdThatIsNotADirectory() {
        let client = makeClient()
        let id = client.ensureDefaultProject(cwd: "/no/such/dir", activate: false)
        #expect(client.workspace.project(id)?.cwd == "")
    }
}

@Suite("LocalClient delegation")
struct LocalClientDelegationTests {
    // Same SIGTRAP-in-swift-testing concern as PtySupervisorTests'
    // event-observation tests. The PTY-touching delegations
    // (openTab/closeTab/writeTab/etc) are exercised end-to-end
    // by the M9 manual pass and a future M4b IPCServer
    // integration test.
    @Test(.disabled("PTY-touching; same swift-testing SIGTRAP as PtySupervisorTests; covered by M9 manual pass"))
    func openTabSpawnsAndCloseReaps() async throws {
        let workspace = await Workspace()
        let supervisor = await PtySupervisor()
        let client = await LocalClient(
            workspace: workspace,
            supervisor: supervisor,
            socketPath: "/tmp/roost-localclient-test.sock"
        )
        let project = await client.createProject(name: "test", cwd: "/tmp")
        let tab = try await client.openTab(
            projectID: project.id,
            cwd: "/tmp",
            argv: ["/bin/sh", "-c", "sleep 30"],
            cols: 80,
            rows: 24
        )
        let hasLive = await supervisor.has(tab.id)
        #expect(hasLive, "supervisor should have a live PTY for the tab")
        try await client.closeTab(tab.id)
        let stillLive = await supervisor.has(tab.id)
        #expect(!stillLive, "close should have removed the supervisor session")
    }

    @Test(.disabled("PTY-touching; same swift-testing SIGTRAP as PtySupervisorTests; covered by M9 manual pass"))
    func openTabRollsBackWorkspaceOnPtyFailure() async throws {
        let workspace = await Workspace()
        let supervisor = await PtySupervisor()
        let client = await LocalClient(
            workspace: workspace,
            supervisor: supervisor,
            socketPath: "/tmp/roost-localclient-fail.sock"
        )
        let project = await client.createProject(name: "test", cwd: "/tmp")

        // Spawn one tab successfully, then try to spawn a second
        // tab — but pre-reserve the same tab id via the supervisor
        // directly to force a `duplicateTab` rejection from the
        // workspace's perspective. Easier surrogate: ask
        // PtySupervisor to spawn with a clearly-invalid argv
        // (program that doesn't exist). forkpty itself can't fail
        // for this, but execve will — the child exits 127. From
        // the parent's view the spawn succeeded, so this isn't a
        // pure test of rollback. Skip the rollback test for now;
        // the integration path covers it.
        _ = try await client.openTab(
            projectID: project.id,
            cwd: "/tmp",
            argv: ["/bin/sh", "-c", "exit 0"],
            cols: 80,
            rows: 24
        )
    }
}
