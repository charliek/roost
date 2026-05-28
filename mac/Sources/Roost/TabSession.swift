// One terminal tab in the Mac UI.
//
// Per-tab state lives here so the window can hold many of these and
// swap which one's `terminalView` is visible.
//
// Each TabSession owns:
//   * A `TerminalView` (the libghostty-vt-backed renderer + key
//     responder for this tab).
//   * A long-running `Task` driven by `runShellSession`. Output
//     drains onto the main actor and into the view; keystrokes
//     captured by `TerminalView.keyDown` flow back out via the
//     keystroke `AsyncStream`.
//   * The workspace-assigned tab id, populated asynchronously once
//     `LocalClient.openTab` returns. `id` is `nil` between session
//     start and that reply; closing during that window can't fire
//     `closeShellTab`, but the supervisor's drain task tears the
//     PTY down regardless when the keystroke stream finishes.
//
// Threading: the session is constructed and torn down on the main
// actor. The two AsyncStreams bridge across the supervisor's
// background DispatchSourceRead queue and the main actor — both
// stream continuations are themselves Sendable so nothing
// non-Sendable crosses the boundary.

import AppKit
import Foundation

@MainActor
final class TabSession {
    /// Initial cell-grid size used to spawn the PTY. The
    /// live grid follows `terminalView.cols / rows`, which update on
    /// window resize (Phase 6a M3 / step 2g).
    let initialCols: UInt16
    let initialRows: UInt16
    let terminalView: TerminalView

    /// Workspace-assigned tab id. `nil` until `LocalClient.openTab`
    /// returns; the `onIDAssigned` callback passed to `start` fires
    /// once it's set.
    private(set) var id: Int64?

    /// Live tab metadata mirrored from `RoostEvent.tabTitle` /
    /// `RoostEvent.tabCwd` / `RoostEvent.tabState` (which the
    /// `Workspace.subscribe` bridge produces). `nil` when no event
    /// has fired yet; the tab pill falls back to "Tab N" labels
    /// until cwd / title arrive.
    var liveTitle: String?
    var liveCwd: String?
    var liveState: Int32?
    /// Phase 6a P7: tracks `TabNotificationEvent.has_pending` so
    /// the tab pill + sidebar row can render an accent badge.
    /// Cleared via `ClearTabNotification` when the user focuses
    /// the tab.
    var liveHasNotification: Bool = false

    /// M6 of `goal-mac-parity-2026-05-18.md`: tracks
    /// `HookActiveChangedEvent.active`. While true, the tab's agent
    /// state is suppressed in the per-project sidebar rollup — the
    /// Claude hook owns the urgency surface and promoting a colored
    /// stripe alongside would double-count it. Mirrors the Linux
    /// `crates/roost-linux/src/rollup.rs` semantics.
    var hookActive: Bool = false

    /// Project the tab belongs to. Set at construction so the window
    /// can filter tabs by project before `start()` ever runs — the
    /// workspace enforces the same id on `openTab`.
    let projectID: Int64

    private var sessionTask: Task<Void, Never>?
    private var outputDrainTask: Task<Void, Never>?
    private var keystrokeContinuation: AsyncStream<PtyClientEvent>.Continuation?

    /// True when this TabSession is *attached* to a tab opened by
    /// another client (e.g. `roostctl tab open` over IPC) rather
    /// than spawned from the UI's own `openNewTab`. `close()`
    /// then skips the `closeShellTab` RPC so quitting the UI
    /// doesn't silently kill an externally-spawned shell. The
    /// attaching caller (`App.swift::handleEvent`'s
    /// `.tabOpened` arm) is responsible for whatever close
    /// semantics it wants.
    private var skipCloseRPC: Bool = false

    /// Last cols/rows we sent through `runShellSession`'s resize
    /// channel. Used to deduplicate resize events that fall within
    /// a single live-resize gesture (NSView `setFrameSize` can fire
    /// dozens of times for one drag, but the grid metric is stable
    /// in chunks).
    private var lastSentCols: UInt16
    private var lastSentRows: UInt16

    init(
        projectID: Int64,
        cols: UInt16 = 80,
        rows: UInt16 = 24,
        theme: Theme = .fallback,
        font: NSFont = NSFont.monospacedSystemFont(ofSize: 14, weight: .regular),
        copyOnSelect: CopyOnSelect = .default,
        clipboardWrite: ClipboardWrite = .default
    ) {
        self.projectID = projectID
        self.initialCols = cols
        self.initialRows = rows
        self.lastSentCols = cols
        self.lastSentRows = rows
        self.terminalView = TerminalView(
            cols: cols,
            rows: rows,
            theme: theme,
            font: font,
            copyOnSelect: copyOnSelect,
            clipboardWrite: clipboardWrite
        )
    }

    /// Send a PTY resize event upstream. Called by the TerminalView
    /// host whenever the cell grid changes size due to a window
    /// resize. Drops no-op resizes (same dimensions as the last sent
    /// pair) so the writer loop doesn't get hammered during live
    /// drag.
    @MainActor
    func resize(cols: UInt16, rows: UInt16) {
        guard cols > 0, rows > 0 else { return }
        guard cols != lastSentCols || rows != lastSentRows else { return }
        lastSentCols = cols
        lastSentRows = rows
        keystrokeContinuation?.yield(.resize(cols: cols, rows: rows))
    }

    /// Spin up the PTY session. Safe to call once per TabSession;
    /// calling twice on the same instance is undefined (would leak
    /// the first task). The window-level code only ever calls this
    /// in the same turn the TabSession is allocated.
    ///
    /// `onIDAssigned` lets the window splice the new tab into the
    /// tab bar with its real workspace-assigned id. Fires on the
    /// main actor.
    func start(
        socketPath: String,
        title: String,
        cwd: String = "",
        argv: [String] = [],
        onIDAssigned: @escaping @MainActor (Int64) -> Void
    ) {
        let (keystrokes, kCont) = AsyncStream<PtyClientEvent>.makeStream()
        let (output, oCont) = AsyncStream<Data>.makeStream()
        self.keystrokeContinuation = kCont

        terminalView.onKey = { [weak self] data in
            // ROOST_TEST_MODE=1 tap: mirror outbound input bytes
            // (keystrokes, paste, OSC reply replies — every byte
            // appendBytes' onKey closure produces) into the per-tab
            // capture buffer hung off RoostBackend.shared. Single
            // tap point catches everything because the Mac OSC
            // reply path also routes through onKey
            // (TerminalView.appendBytes:366). The buffer is
            // allocated lazily on first read by the IPC handler.
            if let self, let tabID = self.id,
               let buf = RoostBackend.shared.ensureInputCapture(tabID: tabID)
            {
                buf.append(data)
            }
            self?.keystrokeContinuation?.yield(.input(data))
        }
        terminalView.onResize = { [weak self] cols, rows in
            self?.resize(cols: cols, rows: rows)
        }
        // Each OSC event the scanner lifts out of the PTY byte
        // stream routes through `reportOsc`, which calls
        // `LocalClient.applyOSC` in-process. `tabID` may still be
        // nil when the very first PTY bytes arrive (openTab hasn't
        // returned yet) — skip in that case; the next chunk will
        // catch any subsequent OSCs.
        let socket = socketPath
        terminalView.onOsc = { [weak self] cmd, payload in
            guard let self, let tabID = self.id else { return }
            Task.detached {
                await reportOsc(
                    socketPath: socket,
                    tabID: tabID,
                    oscCommand: cmd,
                    payload: payload
                )
            }
        }

        outputDrainTask = Task { @MainActor [weak self] in
            for await chunk in output {
                self?.terminalView.appendBytes(chunk)
            }
        }

        let cols = self.initialCols
        let rows = self.initialRows
        let projectID = self.projectID
        sessionTask = Task {
            await runShellSession(
                socketPath: socketPath,
                projectID: projectID,
                cwd: cwd,
                cols: cols,
                rows: rows,
                title: title,
                argv: argv,
                keystrokes: keystrokes,
                onTabOpened: { tabID in
                    Task { @MainActor [weak self] in
                        self?.id = tabID
                        onIDAssigned(tabID)
                    }
                }
            ) { data in
                oCont.yield(data)
            }
            oCont.finish()
        }
    }

    /// Attach to a workspace tab that was opened by a different
    /// client (e.g. `roostctl tab open` over IPC). Mirrors
    /// `start()` but skips the `LocalClient.openTab` call — the
    /// workspace tab + supervisor PTY already exist, and we just
    /// plug a TerminalView in front of the running session.
    ///
    /// `tabID` is pre-known (came from the `.tabOpened` event).
    /// `closeOwnedByExternal` controls whether tearing down this
    /// TabSession also closes the workspace tab: pass `false` for
    /// externally-opened tabs so e.g. quitting the UI doesn't
    /// silently kill `roostctl`-spawned shells.
    func attach(
        socketPath: String,
        tabID: Int64,
        closeOwnedByExternal: Bool = true
    ) {
        self.id = tabID
        let (keystrokes, kCont) = AsyncStream<PtyClientEvent>.makeStream()
        let (output, oCont) = AsyncStream<Data>.makeStream()
        self.keystrokeContinuation = kCont
        self.skipCloseRPC = closeOwnedByExternal

        terminalView.onKey = { [weak self] data in
            // ROOST_TEST_MODE=1 tap: same shape as in `start()`
            // above. The two onKey installations are kept in lock-
            // step — any future change to one must mirror to the
            // other or `tab.capture_pty_input` will miss bytes for
            // attach-flow tabs.
            if let self, let tabID = self.id,
               let buf = RoostBackend.shared.ensureInputCapture(tabID: tabID)
            {
                buf.append(data)
            }
            self?.keystrokeContinuation?.yield(.input(data))
        }
        terminalView.onResize = { [weak self] cols, rows in
            self?.resize(cols: cols, rows: rows)
        }
        let socket = socketPath
        terminalView.onOsc = { [weak self] cmd, payload in
            guard let self, let tabID = self.id else { return }
            Task.detached {
                await reportOsc(
                    socketPath: socket,
                    tabID: tabID,
                    oscCommand: cmd,
                    payload: payload
                )
            }
        }

        outputDrainTask = Task { @MainActor [weak self] in
            for await chunk in output {
                self?.terminalView.appendBytes(chunk)
            }
        }

        let cols = self.initialCols
        let rows = self.initialRows
        sessionTask = Task {
            await attachShellSession(
                socketPath: socketPath,
                tabID: tabID,
                cols: cols,
                rows: rows,
                keystrokes: keystrokes
            ) { data in
                oCont.yield(data)
            }
            oCont.finish()
        }
    }

    /// Tear down the session. Closes the keystroke stream (which
    /// ends `runShellSession`'s writer loop), cancels the session
    /// task, and fires a best-effort `closeShellTab` if we know
    /// our id (`LocalClient.closeTab` is in-process and idempotent).
    func close(socketPath: String) {
        keystrokeContinuation?.finish()
        keystrokeContinuation = nil
        terminalView.onKey = nil
        sessionTask?.cancel()
        sessionTask = nil
        outputDrainTask?.cancel()
        outputDrainTask = nil
        let skipRPC = skipCloseRPC
        if let id = self.id {
            self.id = nil
            // Drop any input-capture buffer that test mode allocated
            // for this tab. No-op outside ROOST_TEST_MODE=1.
            RoostBackend.shared.dropInputCapture(tabID: id)
            // For externally-opened tabs (`skipCloseRPC == true`),
            // don't fire the LocalClient.closeTab RPC — the
            // opening client (`roostctl`, Claude hook) owns the
            // tab's lifetime. We just detach the UI view.
            if !skipRPC {
                Task.detached {
                    await closeShellTab(socketPath: socketPath, tabID: id)
                }
            }
        }
    }
}
