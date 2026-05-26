// RoostClient.swift — daemon-removal refactor M4b3b.
//
// The free functions in this file used to dial a gRPC server (the
// `roost-core` daemon over a Unix-domain socket) for every workspace
// or PTY operation. Since M4 the same operations are served by the
// in-process `LocalClient` held on `RoostBackend.shared`, so each
// function here is now a thin adapter that hops to the main actor
// and delegates. The signatures match the gRPC-era versions so the
// App.swift call sites need no churn beyond the `socketPath`
// parameter — which is retained, ignored in the call body, and
// preserved purely so the migration is a one-file diff.
//
// `runShellSession` is the most involved adapter: it opens an
// in-process tab via `LocalClient.openTab` (which spawns a
// `PtySupervisor` PTY), subscribes to the supervisor's event sink
// to forward output bytes, and pumps the caller's keystroke stream
// back into `LocalClient.writeTab` / `resizeTab`. Loop ends when
// either the keystroke stream finishes (caller-initiated close) or
// the supervisor emits `tabExited` (shell-initiated exit).

import Foundation

// MARK: - Identify

/// A snapshot of the local roost instance's identity. In the gRPC
/// era this came from the daemon's `Identify` RPC; in-process we
/// synthesize it from the current process state. The shape stays
/// stable so the UI's reachability + version-banner code path
/// doesn't change.
struct RoostIdentity: Sendable {
    let socketPath: String
    let pid: Int32
    let activeProjectID: Int64
    let activeTabID: Int64
    let daemonVersion: String
    let protocolVersion: UInt32
}

enum IdentifyOutcome: Sendable {
    case ok(RoostIdentity)
    case failed(String)
}

/// In-process "handshake". The previous gRPC implementation had a
/// 5-second timeout because the daemon could be in any state; the
/// in-process version is a single main-actor hop that just reads
/// the Workspace's active selection. Kept `async` so existing
/// callers don't change shape.
func runIdentify(socketPath: String) async -> IdentifyOutcome {
    let active: (Int64, Int64)? = await MainActor.run {
        guard let workspace = RoostBackend.shared.workspace else { return nil }
        return (workspace.activeProjectID, workspace.activeTabID)
    }
    guard let (activeProject, activeTab) = active else {
        return .failed("RoostBackend has not started — workspace unavailable")
    }
    return .ok(
        RoostIdentity(
            socketPath: socketPath,
            pid: getpid(),
            activeProjectID: activeProject,
            activeTabID: activeTab,
            daemonVersion: clientVersion(),
            protocolVersion: UInt32(ipcProtocolVersion)
        )
    )
}

private func clientVersion() -> String {
    Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "0.1.0"
}

// MARK: - Outbound PTY events the keystroke pump understands

/// Phase 6a M3 lifts the previous "stream is just keystroke bytes"
/// shape so window-resize can ride the same channel as input.
enum PtyClientEvent: Sendable {
    case input(Data)
    case resize(cols: UInt16, rows: UInt16)
}

// MARK: - Shell session lifecycle

/// Open a fresh tab in the in-process workspace, spawn its PTY, and
/// drive a bidirectional flow:
///   * The `PtySupervisor`'s byte events for this tab are delivered
///     to `onOutput`.
///   * The supplied `keystrokes` stream is drained on the main actor
///     and forwarded to the supervisor via `writeTab` / `resizeTab`.
///
/// Returns when either the keystroke stream finishes (caller-side
/// close) or the supervisor emits `tabExited` for the spawned tab
/// (shell-side exit / forced reap). On exit we close the supervisor
/// tab + workspace row to guarantee no orphaned PTY survives a
/// half-shutdown.
///
/// `onTabOpened` fires once with the workspace-assigned tab id as
/// soon as the workspace insert returns, before any output arrives.
/// Both callbacks may run on the main actor; consumers that touch
/// AppKit views from them should not hop again.
func runShellSession(
    socketPath: String,
    projectID: Int64 = 0,
    cwd: String = "",
    cols: UInt16 = 80,
    rows: UInt16 = 24,
    title: String = "roost-mac",
    argv: [String] = [],
    keystrokes: AsyncStream<PtyClientEvent>,
    onTabOpened: @escaping @Sendable (Int64) -> Void,
    onOutput: @escaping @Sendable (Data) -> Void
) async {
    // Open the tab + spawn the PTY on the main actor. The
    // supervisor subscription is installed *before* `openTab` so
    // the byte stream can't race the subscription on the very first
    // read; the supervisor only fires events to subscribers it
    // already knows about.
    let opened: (Int64, UUID, AsyncStream<Void>)?
    let exitContinuation: AsyncStream<Void>.Continuation?
    (opened, exitContinuation) = await MainActor.run {
        () -> ((Int64, UUID, AsyncStream<Void>)?, AsyncStream<Void>.Continuation?) in
        guard let localClient = RoostBackend.shared.localClient,
              let supervisor = RoostBackend.shared.supervisor
        else {
            return (nil, nil)
        }

        let (exitStream, exitCont) = AsyncStream<Void>.makeStream()

        // Subscribe with a placeholder id captured by reference so
        // the handler can compare once openTab returns. We
        // intentionally do not unwrap the optional until openTab
        // succeeds — pre-open events are ignored.
        let tabIDBox = TabIDBox()
        // The supervisor's subscriber handler is fired from the
        // supervisor's @MainActor emit() (see PtySupervisor.swift:80),
        // so the closure body actually executes on the main actor —
        // but Swift's @Sendable annotation hides that from the
        // compiler. `MainActor.assumeIsolated` documents the
        // invariant and lets us read the @MainActor-isolated
        // TabIDBox.id directly without an extra hop.
        let token = supervisor.subscribe { event in
            MainActor.assumeIsolated {
                guard let myID = tabIDBox.id else { return }
                switch event {
                case .bytes(let id, let data) where id == myID:
                    onOutput(data)
                case .tabExited(let id, _) where id == myID:
                    exitCont.yield(())
                    exitCont.finish()
                default:
                    break
                }
            }
        }

        do {
            let tab = try localClient.openTab(
                projectID: projectID,
                cwd: cwd,
                argv: argv,
                cols: cols,
                rows: rows,
                title: title
            )
            tabIDBox.id = tab.id
            onTabOpened(tab.id)
            return ((tab.id, token, exitStream), exitCont)
        } catch {
            supervisor.unsubscribe(token: token)
            exitCont.finish()
            RoostLogger.shared.warn("openTab failed: \(error)")
            return (nil, nil)
        }
    }

    guard let (tabID, token, exitStream) = opened else {
        return
    }

    // Race the keystroke pump against the supervisor's tabExited
    // signal so a shell that exits doesn't leave us waiting on a
    // keystroke stream that has no producer left.
    await withTaskGroup(of: Void.self) { group in
        group.addTask {
            for await _ in exitStream {
                return
            }
        }
        group.addTask {
            for await event in keystrokes {
                await MainActor.run {
                    guard let localClient = RoostBackend.shared.localClient else {
                        return
                    }
                    do {
                        switch event {
                        case .input(let data):
                            _ = try localClient.writeTab(tabID, data: data)
                        case .resize(let cols, let rows):
                            try localClient.resizeTab(tabID, cols: cols, rows: rows)
                        }
                    } catch {
                        RoostLogger.shared.warn("pty event failed (tab \(tabID)): \(error)")
                    }
                }
            }
            // Keystroke stream finished — make sure the exit-side
            // task wakes up too so we don't deadlock on the
            // group-next() below.
            exitContinuation?.finish()
        }
        _ = await group.next()
        group.cancelAll()
    }

    // Tear down the supervisor subscription + close the tab.
    // Workspace.closeTab is idempotent (no-ops on already-gone id);
    // supervisor.close is too.
    await MainActor.run {
        if let supervisor = RoostBackend.shared.supervisor {
            supervisor.unsubscribe(token: token)
        }
        if let localClient = RoostBackend.shared.localClient {
            try? localClient.closeTab(tabID)
        }
    }
}

/// Helper box so the supervisor subscriber installed pre-`openTab`
/// can pick up the tab id once it's known. Single-actor (main),
/// single-writer — no synchronization needed.
@MainActor
private final class TabIDBox {
    var id: Int64?
}

/// Attach to a workspace tab that was opened by a *different*
/// client (e.g. `roostctl tab open`) — the workspace + PTY
/// already exist, so we skip the `openTab` call and just plug a
/// keystroke/output pump into the running supervisor session.
/// Mirrors `runShellSession` for everything *after* the open.
///
/// Returns when either the keystroke stream finishes or the
/// supervisor's `tabExited` fires for this tab id. Unlike
/// `runShellSession`, we do NOT call `localClient.closeTab` on
/// exit — the tab's owner (whatever opened it) is responsible
/// for the close, and tearing it down from the UI would surprise
/// `roostctl` callers who expect their tab to outlive a UI
/// inspection.
func attachShellSession(
    socketPath: String,
    tabID: Int64,
    cols: UInt16 = 80,
    rows: UInt16 = 24,
    keystrokes: AsyncStream<PtyClientEvent>,
    onOutput: @escaping @Sendable (Data) -> Void
) async {
    // Set up the supervisor subscription synchronously on the
    // main actor so we don't miss bytes between subscribe and
    // the first drain iteration.
    let setupResult: (UUID, AsyncStream<Void>, AsyncStream<Void>.Continuation)?
    setupResult = await MainActor.run {
        () -> (UUID, AsyncStream<Void>, AsyncStream<Void>.Continuation)? in
        guard let supervisor = RoostBackend.shared.supervisor else { return nil }
        // The tab must already exist in the supervisor (the
        // cross-client `tab.open` IPC op went through the same
        // LocalClient.openTab path on its way in, which spawned
        // the PTY). If not, bail — the caller will have already
        // logged the missing-tab case.
        guard supervisor.has(tabID) else { return nil }

        let (exitStream, exitCont) = AsyncStream<Void>.makeStream()
        let token = supervisor.subscribe { event in
            MainActor.assumeIsolated {
                switch event {
                case .bytes(let id, let data) where id == tabID:
                    onOutput(data)
                case .tabExited(let id, _) where id == tabID:
                    exitCont.yield(())
                    exitCont.finish()
                default:
                    break
                }
            }
        }
        return (token, exitStream, exitCont)
    }

    guard let (token, exitStream, exitContinuation) = setupResult else {
        return
    }

    // Wire the initial size to the existing PTY in case the
    // attaching UI's cell grid differs from whatever the tab was
    // opened with.
    await MainActor.run {
        try? RoostBackend.shared.localClient?.resizeTab(tabID, cols: cols, rows: rows)
    }

    await withTaskGroup(of: Void.self) { group in
        group.addTask {
            for await _ in exitStream { return }
        }
        group.addTask {
            for await event in keystrokes {
                await MainActor.run {
                    guard let localClient = RoostBackend.shared.localClient else { return }
                    do {
                        switch event {
                        case .input(let data):
                            _ = try localClient.writeTab(tabID, data: data)
                        case .resize(let cols, let rows):
                            try localClient.resizeTab(tabID, cols: cols, rows: rows)
                        }
                    } catch {
                        RoostLogger.shared.warn("attached pty event failed (tab \(tabID)): \(error)")
                    }
                }
            }
            exitContinuation.finish()
        }
        _ = await group.next()
        group.cancelAll()
    }

    await MainActor.run {
        RoostBackend.shared.supervisor?.unsubscribe(token: token)
    }
}

/// Best-effort tab close. Used when the UI closes a tab so the
/// supervisor reaps the child immediately rather than waiting for
/// the keystroke stream to drain. In-process this is a direct
/// LocalClient.closeTab — the daemon's CloseTab RPC is gone.
func closeShellTab(socketPath: String, tabID: Int64) async {
    await MainActor.run {
        do {
            try RoostBackend.shared.localClient?.closeTab(tabID)
        } catch {
            RoostLogger.shared.warn("closeTab(\(tabID)) failed: \(error)")
        }
    }
}

// MARK: - Project lifecycle

/// Plain-Swift mirror of the workspace `Project` so the UI can hold
/// a list without leaning on workspace types in its view model.
/// Tabs are intentionally not modeled here — the UI tracks its own
/// `TabSession` instances in `RoostApp`.
struct ProjectSnapshot: Sendable, Hashable {
    let id: Int64
    let name: String
    let cwd: String
}

/// Fetch the in-process tab count for a single project. Returns
/// `nil` when the backend hasn't booted yet so the caller falls
/// back conservatively (the gRPC-era version returned `nil` on RPC
/// failure — the meaning is "unknown, defend yourself").
func daemonTabCount(socketPath: String, projectID: Int64) async -> Int? {
    await MainActor.run {
        guard let workspace = RoostBackend.shared.workspace else { return nil }
        return workspace.tabs(in: projectID).count
    }
}

/// Fetch the full project list. Returns `[]` if the backend hasn't
/// booted (matches the gRPC-era "any error" fallback).
func listProjects(socketPath: String) async -> [ProjectSnapshot] {
    await MainActor.run {
        guard let localClient = RoostBackend.shared.localClient else { return [] }
        return localClient.listProjects().map {
            ProjectSnapshot(id: $0.id, name: $0.name, cwd: $0.cwd)
        }
    }
}

/// Best-effort create. `name = ""` → workspace picks `"Untitled <n>"`.
func createProject(socketPath: String, name: String, cwd: String) async -> ProjectSnapshot? {
    await MainActor.run {
        guard let localClient = RoostBackend.shared.localClient else { return nil }
        let project = localClient.createProject(name: name, cwd: cwd)
        return ProjectSnapshot(id: project.id, name: project.name, cwd: project.cwd)
    }
}

/// Best-effort tab rename. The workspace handler sets the per-tab
/// `userTitled` lock so subsequent OSC 0/1/2 emissions stop
/// overwriting (matches the daemon-era `user_titled` semantics).
func setTabTitle(socketPath: String, tabID: Int64, title: String) async {
    await MainActor.run {
        do {
            try RoostBackend.shared.localClient?.setTabTitle(tabID, title: title)
        } catch {
            RoostLogger.shared.warn("setTabTitle(\(tabID)) failed: \(error)")
        }
    }
}

func renameProject(socketPath: String, projectID: Int64, name: String) async {
    await MainActor.run {
        do {
            try RoostBackend.shared.localClient?.renameProject(projectID, name: name)
        } catch {
            RoostLogger.shared.warn("renameProject(\(projectID)) failed: \(error)")
        }
    }
}

/// Persist a new tab order within a project. The workspace
/// validates that every id in `tabIDs` belongs to `projectID`;
/// unknown ids throw and the call logs.
func reorderTabs(socketPath: String, projectID: Int64, tabIDs: [Int64]) async {
    await MainActor.run {
        do {
            try RoostBackend.shared.localClient?.reorderTabs(projectID: projectID, tabIDs: tabIDs)
        } catch {
            RoostLogger.shared.warn("reorderTabs(\(projectID)) failed: \(error)")
        }
    }
}

func reorderProjects(socketPath: String, projectIDs: [Int64]) async {
    await MainActor.run {
        do {
            try RoostBackend.shared.localClient?.reorderProjects(projectIDs)
        } catch {
            RoostLogger.shared.warn("reorderProjects failed: \(error)")
        }
    }
}

// MARK: - Workspace event stream

/// Long-lived stream of workspace mutation events. In the gRPC era
/// this was a server-stream over UDS; in-process we subscribe
/// directly to `Workspace`'s observer list and convert each
/// `Workspace.Event` to the transport-shaped `RoostEvent` consumed
/// by App.swift's `handleEvent`.
///
/// The stream finishes when the consumer cancels (the `onTermination`
/// hook unsubscribes from the workspace).
func watchEvents(socketPath: String) -> AsyncStream<RoostEvent> {
    AsyncStream { continuation in
        let tokenBox = WatchTokenBox()
        Task { @MainActor in
            guard let workspace = RoostBackend.shared.workspace else {
                continuation.finish()
                return
            }
            let token = workspace.subscribe { event in
                Task { @MainActor in
                    guard let workspace = RoostBackend.shared.workspace else { return }
                    continuation.yield(event.toRoostEvent(workspace: workspace))
                }
            }
            tokenBox.token = token
            // The subscription is now live; everything from here is
            // buffered by the stream. Reconcile first so a tab opened
            // before this point (boot gap, or a reconnect) still
            // materializes — see `RoostEvent.resync`. The async-hop
            // registration above is exactly why a synchronous reconcile
            // at bootstrap would race; ordering it here can't.
            continuation.yield(.resync)
        }
        continuation.onTermination = { _ in
            Task { @MainActor in
                if let token = tokenBox.token {
                    RoostBackend.shared.workspace?.unsubscribe(token: token)
                }
            }
        }
    }
}

@MainActor
private final class WatchTokenBox {
    var token: UUID?
}

/// Best-effort `clearTabNotification`. Mirrors the workspace handler
/// of the same name — fires `TabNotification(hasPending: false)` so
/// the badge clears on every observer (the IPC server's
/// `events.subscribe` consumers see it too).
func clearTabNotification(socketPath: String, tabID: Int64) async {
    await MainActor.run {
        do {
            try RoostBackend.shared.localClient?.clearTabNotification(tabID)
        } catch {
            RoostLogger.shared.warn("clearTabNotification(\(tabID)) failed: \(error)")
        }
    }
}

/// Apply a parsed OSC sequence to the workspace. The UI's
/// OscScanner extracts `(command, payload)` pairs from the PTY byte
/// stream and dispatches them via this fn. In-process this routes
/// through `LocalClient.applyOSC`, which encapsulates the
/// per-command translation (title / cwd / notification) so the UI
/// doesn't need to peek at the command number.
func reportOsc(
    socketPath: String,
    tabID: Int64,
    oscCommand: UInt32,
    payload: String
) async {
    await MainActor.run {
        RoostBackend.shared.localClient?.applyOSC(
            tabID: tabID,
            command: oscCommand,
            payload: payload
        )
    }
}

/// Best-effort `deleteProject`. The workspace cascade-closes every
/// tab belonging to the project; the supervisor reaps their PTYs
/// inside `LocalClient.deleteProject`.
func deleteProject(socketPath: String, projectID: Int64) async {
    await MainActor.run {
        do {
            _ = try RoostBackend.shared.localClient?.deleteProject(projectID)
        } catch {
            RoostLogger.shared.warn("deleteProject(\(projectID)) failed: \(error)")
        }
    }
}
