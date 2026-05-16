// One terminal tab in the Mac UI.
//
// Phase 6a step 1: per-tab state lives here so the window can hold
// many of these and swap which one's `terminalView` is visible.
//
// Each TabSession owns:
//   * A `TerminalView` (the libghostty-vt-backed renderer + key
//     responder for this tab).
//   * A long-running gRPC `StreamPty` task driven by
//     `runShellSession`. Output drains onto the main actor and
//     into the view; keystrokes captured by `TerminalView.keyDown`
//     flow back out via the keystroke `AsyncStream`.
//   * The daemon-assigned tab id, populated asynchronously once
//     `OpenTab` returns. `id` is `nil` between session start and
//     the daemon's reply; closing during that window can't issue
//     a `CloseTab` RPC, but the daemon will reap the PTY when our
//     StreamPty stream ends regardless.
//
// Threading: the session is constructed and torn down on the main
// actor. The two AsyncStreams bridge across the gRPC background
// task — the same pattern documented in `RoostApp.startShellSession`
// before the multi-tab refactor: nothing non-Sendable crosses the
// boundary because both stream continuations are themselves Sendable.

import AppKit
import Foundation

@MainActor
final class TabSession {
    /// Initial cell-grid size used to spawn the daemon-side PTY. The
    /// live grid follows `terminalView.cols / rows`, which update on
    /// window resize (Phase 6a M3 / step 2g).
    let initialCols: UInt16
    let initialRows: UInt16
    let terminalView: TerminalView

    /// Daemon-assigned tab id. `nil` until `OpenTab` returns; the
    /// `onIDAssigned` callback passed to `start` fires once it's set.
    private(set) var id: Int64?

    /// Project the tab belongs to. Set at construction so the window
    /// can filter tabs by project before `start()` ever runs — the
    /// daemon enforces the same id on `OpenTab`.
    let projectID: Int64

    private var sessionTask: Task<Void, Never>?
    private var outputDrainTask: Task<Void, Never>?
    private var keystrokeContinuation: AsyncStream<PtyClientEvent>.Continuation?

    /// Last cols/rows we sent to the daemon. Used to deduplicate
    /// resize events that fall within a single live-resize gesture
    /// (NSView `setFrameSize` can fire dozens of times for one
    /// drag, but the grid metric is stable in chunks).
    private var lastSentCols: UInt16
    private var lastSentRows: UInt16

    init(projectID: Int64, cols: UInt16 = 80, rows: UInt16 = 24) {
        self.projectID = projectID
        self.initialCols = cols
        self.initialRows = rows
        self.lastSentCols = cols
        self.lastSentRows = rows
        self.terminalView = TerminalView(cols: cols, rows: rows)
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

    /// Spin up the StreamPty session. Safe to call once per
    /// TabSession; calling twice on the same instance is undefined
    /// (would leak the first task). The window-level code only ever
    /// calls this in the same turn the TabSession is allocated.
    ///
    /// `onIDAssigned` lets the window splice the new tab into the tab
    /// bar with its real daemon id. Fires on the main actor.
    func start(
        socketPath: String,
        title: String,
        onIDAssigned: @escaping @MainActor (Int64) -> Void
    ) {
        let (keystrokes, kCont) = AsyncStream<PtyClientEvent>.makeStream()
        let (output, oCont) = AsyncStream<Data>.makeStream()
        self.keystrokeContinuation = kCont

        terminalView.onKey = { [weak self] data in
            self?.keystrokeContinuation?.yield(.input(data))
        }
        terminalView.onResize = { [weak self] cols, rows in
            self?.resize(cols: cols, rows: rows)
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
                cols: cols,
                rows: rows,
                title: title,
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

    /// Tear down the session. Closes the keystroke stream (which
    /// ends the StreamPty writer), cancels the gRPC task, and fires
    /// a best-effort `CloseTab` RPC to the daemon if we know our id.
    func close(socketPath: String) {
        keystrokeContinuation?.finish()
        keystrokeContinuation = nil
        terminalView.onKey = nil
        sessionTask?.cancel()
        sessionTask = nil
        outputDrainTask?.cancel()
        outputDrainTask = nil
        if let id = self.id {
            self.id = nil
            Task.detached {
                await closeShellTab(socketPath: socketPath, tabID: id)
            }
        }
    }
}
