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

    /// True iff the caller has confirmed (via M4c's
    /// `SingleInstance.acquire(...).acquired`) that we own the
    /// flock at the bundle profile's lock path. When set, the
    /// IPC server is allowed to recover a stale socket left by a
    /// previously kill -9'd instance (M6).
    private var holdsSingleInstanceLock = false

    private init() {}

    /// Stand up the in-process workspace + PTY supervisor and bind
    /// the JSON IPC server on `profile.socketPath`. Idempotent —
    /// safe to call from `applicationDidFinishLaunching` once.
    ///
    /// Pass `holdsSingleInstanceLock: true` iff the caller already
    /// acquired the M4c `SingleInstance` flock. With the lock held
    /// the M6 stale-socket recovery is safe (no live writer can
    /// race the unlink); without it, `EADDRINUSE` surfaces as
    /// `.alreadyBound` so we don't steal someone else's socket.
    func start(profile: BundleProfile, holdsSingleInstanceLock: Bool = false) {
        if started { return }
        started = true
        self.holdsSingleInstanceLock = holdsSingleInstanceLock

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

        // Construct + start the IPC server. If the canonical
        // socket path is already in use (the daemon owns it
        // during the M4b3a parallel-run window) we log + skip
        // rather than steal the path out from under it. CR
        // (codex) flagged that the prior auto-unlink-before-bind
        // behavior would break gRPC bootstrap.
        do {
            let handler = IPCHandlerImpl(
                client: client,
                socketPath: profile.socketPath,
                appLabel: profile.appLabel,
                appID: profile.appID
            )
            let server = try IPCServer(
                socketPath: profile.socketPath,
                handler: handler,
                recoverStaleSocket: holdsSingleInstanceLock
            )
            server.start()
            self.ipcServer = server
            NSLog("roost-ipc: server bound at \(profile.socketPath)")
        } catch let err as IPCServerError {
            if case .alreadyBound = err {
                NSLog(
                    "roost-ipc: socket at \(profile.socketPath) already in use; assuming daemon is running, skipping IPC server bind (M4b3a transitional state)"
                )
            } else {
                NSLog("roost-ipc: failed to bind IPC server at \(profile.socketPath): \(err)")
            }
        } catch {
            NSLog("roost-ipc: failed to bind IPC server at \(profile.socketPath): \(error)")
        }
    }
}
