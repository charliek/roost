// RoostBackend.swift — daemon-removal refactor M4b.
//
// Process-wide singleton that owns the post-daemon
// infrastructure: Workspace + PtySupervisor + LocalClient +
// IPCHandlerImpl + IPCServer. Booted from
// `RoostApp.applicationDidFinishLaunching` so `roostctl` and
// Claude hooks see a live IPC socket immediately after the app
// finishes launching.
//
// M4b3a (this commit): the backend stands up alongside the
// existing gRPC client. The UI continues to source its state
// from the daemon over gRPC. The IPC server therefore serves an
// initially-empty in-process Workspace — useful for verifying
// the wire format end-to-end against `roostctl identify`, but
// not yet the canonical state.
//
// M4b3b will rewire `RoostClient.swift`'s top-level functions
// (`createProject`, `openTab`, `watchEvents`, etc.) onto the
// `LocalClient` held here, at which point the in-process
// Workspace becomes the source of truth and the daemon goes
// quiet.

import Foundation

@MainActor
final class RoostBackend {
    static let shared = RoostBackend()

    private(set) var workspace: Workspace?
    private(set) var supervisor: PtySupervisor?
    private(set) var localClient: LocalClient?
    private var ipcServer: IPCServer?
    private var started = false

    private init() {}

    /// Stand up the in-process workspace + PTY supervisor and bind
    /// the JSON IPC server on `profile.socketPath`. Idempotent —
    /// safe to call from `applicationDidFinishLaunching` once.
    func start(profile: BundleProfile) {
        if started { return }
        started = true

        let workspace = Workspace(statePath: profile.stateJSONPath)
        let supervisor = PtySupervisor()
        let client = LocalClient(
            workspace: workspace,
            supervisor: supervisor,
            socketPath: profile.socketPath
        )

        self.workspace = workspace
        self.supervisor = supervisor
        self.localClient = client

        // Best-effort: ensure the parent dirs exist before
        // bind/state writes try to. Workspace already does this
        // lazily on first write; doing it once at boot keeps the
        // diagnostics predictable.
        let stateParent = (profile.stateJSONPath as NSString).deletingLastPathComponent
        let socketParent = (profile.socketPath as NSString).deletingLastPathComponent
        let logParent = (profile.logDir as NSString)
        for dir in [stateParent, socketParent, logParent as String] {
            try? FileManager.default.createDirectory(
                atPath: dir,
                withIntermediateDirectories: true
            )
        }

        // Construct + start the IPC server.
        do {
            let handler = IPCHandlerImpl(
                client: client,
                socketPath: profile.socketPath,
                appLabel: profile.appLabel,
                appID: profile.appID
            )
            let server = try IPCServer(socketPath: profile.socketPath, handler: handler)
            server.start()
            self.ipcServer = server
            NSLog("roost-ipc: server bound at \(profile.socketPath)")
        } catch {
            NSLog("roost-ipc: failed to bind IPC server at \(profile.socketPath): \(error)")
        }
    }
}
