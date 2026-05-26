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

import AppKit
import Darwin
import Foundation

/// The running UI, as seen by the IPC handler. One seam for every
/// main-thread-only op so the handler never reaches for `NSApp.delegate`
/// or pokes AppKit directly. `RoostApp` is the only conformer; the
/// GTK side's equivalent is the `UiRequest` channel
/// (`crates/roost-linux/src/ipc.rs`).
@MainActor
protocol UiBridge: AnyObject {
    /// Main window, for whole-window ops (`app.screenshot`).
    var mainWindow: NSWindow? { get }
    /// Read a tab's terminal viewport as text (`tab.dump`); `nil` when
    /// no live tab holds that id.
    func dumpTab(tabID: Int64) -> TerminalView.Dump?

    // Command-palette drive surface (`palette.*` ops). The GTK side's
    // equivalent is the `UiRequest::Palette*` arms. Each returns the
    // resulting palette state; `paletteActivate` returns `nil` when no
    // palette is open or no visible row matched (→ `not-found`).
    func openPalette(kind: String) -> PaletteSnapshot
    func paletteState() -> PaletteSnapshot
    func paletteQuery(_ text: String) -> PaletteSnapshot
    func paletteActivate(id: String) -> PaletteSnapshot?
    func dismissPaletteOverlay() -> PaletteSnapshot
}

/// A read of the command-palette overlay for the `palette.*` IPC ops,
/// built by `RoostApp` and mapped to the wire result by the IPC handler.
/// Mirrors `roost_ipc::messages::PaletteStateResult`. `open == false` is
/// the closed palette (the other fields are then empty/default).
struct PaletteSnapshot: Sendable {
    var open: Bool
    var frame: String?
    var query: String
    var selection: Int
    var items: [Item]

    struct Item: Sendable {
        let id: String
        let title: String
        let subtitle: String?
    }

    static let closed = PaletteSnapshot(
        open: false, frame: nil, query: "", selection: 0, items: []
    )
}

@MainActor
final class RoostBackend {
    static let shared = RoostBackend()

    private(set) var workspace: Workspace?
    private(set) var supervisor: PtySupervisor?
    private(set) var localClient: LocalClient?
    private var ipcServer: IPCServer?
    private var started = false
    /// Token for the process-wide PTY-exit subscription installed in
    /// `start`. Kept alive for the backend's lifetime (the whole
    /// process), so it's never explicitly unsubscribed.
    private var supervisorExitToken: UUID?

    /// The running UI, registered by `RoostApp` once it's built. Weak so
    /// the backend singleton never keeps a torn-down UI alive. This is
    /// the single seam the IPC handler uses to reach anything that needs
    /// the main actor (window render for `app.screenshot`, render-state
    /// walk for `tab.dump`) — without the handler holding AppKit refs or
    /// reaching for `NSApp.delegate`.
    private(set) weak var ui: (any UiBridge)?

    /// Called by `RoostApp` after the window is created.
    func registerUI(_ ui: any UiBridge) {
        self.ui = ui
    }

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
    /// One-shot SIGPIPE-to-SIG_IGN installer. Without this, writing
    /// to a Unix-domain socket whose peer has closed its end
    /// raises SIGPIPE and terminates the process. The IPC server's
    /// `writeAll` already checks for `EPIPE` on Darwin.write
    /// failures, so ignoring SIGPIPE leaves all error handling in
    /// the user-space code path. CR-flagged on
    /// `mac/Sources/Roost/IPCServer.swift:263`.
    nonisolated(unsafe) private static var sigpipeInstalled = false
    private static func ignoreSigpipe() {
        guard !sigpipeInstalled else { return }
        sigpipeInstalled = true
        signal(SIGPIPE, SIG_IGN)
    }

    func start(profile: BundleProfile, holdsSingleInstanceLock: Bool = false) {
        Self.ignoreSigpipe()
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

        // Shell-exit → close the tab, deterministically, for *every*
        // tab whose child process dies — UI-spawned (`runShellSession`)
        // or externally-opened via `roostctl tab open`
        // (`attachShellSession`). Without this, an exited shell could
        // linger as a dead tab: `runShellSession` closes its own tab on
        // exit but `attachShellSession` deliberately doesn't, and the
        // round-trip was async. Routing every `.tabExited` through
        // `closeTab` here mirrors the GTK side's
        // `TabOutput::Exit → close_page_for_tab` path, and the
        // cascade in `Workspace.closeTab` then closes the project when
        // it was the last tab. `closeTab` is idempotent — a racing
        // close (e.g. `runShellSession`'s own teardown) throws
        // `tabNotFound`, which we swallow.
        //
        // The supervisor fires this handler from its `@MainActor`
        // `emit`, so the body runs on the main actor; `@Sendable`
        // hides that from the compiler — `assumeIsolated` documents
        // the invariant (same pattern as `runShellSession`).
        supervisorExitToken = supervisor.subscribe { event in
            MainActor.assumeIsolated {
                guard case .tabExited(let tabID, _) = event else { return }
                do {
                    try client.closeTab(tabID)
                } catch Workspace.WorkspaceError.tabNotFound {
                    // Already gone: the idempotent close race (e.g.
                    // `runShellSession`'s own teardown beat us). Expected.
                } catch {
                    // A real cleanup failure would otherwise leave a
                    // dead tab/project stuck with no trace — log it.
                    NSLog("roost-backend: closeTab(\(tabID)) failed after PTY exit: \(error)")
                }
            }
        }

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
