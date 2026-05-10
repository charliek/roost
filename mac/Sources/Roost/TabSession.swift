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
    let cols: UInt16
    let rows: UInt16
    let terminalView: TerminalView

    /// Daemon-assigned tab id. `nil` until `OpenTab` returns; the
    /// `onIDAssigned` callback passed to `start` fires once it's set.
    private(set) var id: Int64?

    private var sessionTask: Task<Void, Never>?
    private var outputDrainTask: Task<Void, Never>?
    private var keystrokeContinuation: AsyncStream<Data>.Continuation?

    init(cols: UInt16 = 80, rows: UInt16 = 24) {
        self.cols = cols
        self.rows = rows
        self.terminalView = TerminalView(cols: cols, rows: rows)
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
        let (keystrokes, kCont) = AsyncStream<Data>.makeStream()
        let (output, oCont) = AsyncStream<Data>.makeStream()
        self.keystrokeContinuation = kCont

        terminalView.onKey = { [weak self] data in
            self?.keystrokeContinuation?.yield(data)
        }

        outputDrainTask = Task { @MainActor [weak self] in
            for await chunk in output {
                self?.terminalView.appendBytes(chunk)
            }
        }

        let cols = self.cols
        let rows = self.rows
        sessionTask = Task {
            await runShellSession(
                socketPath: socketPath,
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
