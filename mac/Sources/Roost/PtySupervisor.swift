// PtySupervisor.swift — daemon-removal refactor M4a.
//
// Greenfield Swift PTY supervisor. Spawns a shell via
// `forkpty(3)` (NOT `posix_spawn` — the latter doesn't allocate
// a PTY); each tab owns one PTY master fd. A
// `DispatchSourceRead` on a background queue drains the master
// fd; the callback hops to the main thread for libghostty-vt
// `vt_write` (held by the renderer).
//
// Threading rules (from CLAUDE.md's Swift threading subsection,
// landed in M7):
//   * libghostty-vt handles + `vt_write` calls: `@MainActor` only.
//   * PTY read from master fd: `DispatchSourceRead` on a
//     background `DispatchQueue`. Hops to `@MainActor` before
//     any `vt_write`.
//   * Write to master fd: from the main actor (no concurrent
//     writes possible per tab; ordering preserved).
//   * Resize: `ioctl(TIOCSWINSZ)` — fires `SIGWINCH` to child.
//   * Exit: `SIGCHLD` + `waitpid(WNOHANG)`. On exit, close
//     master, fire `tabExited` on the supervisor's event sink.
//   * Quit-time reap: iterate all sessions, `SIGHUP` →
//     `waitpid` with timeout → `SIGKILL` fallback. No zombies.
//   * Env: `ROOST_TAB_ID` + `ROOST_SOCKET` + `TERM` +
//     `COLORTERM=truecolor` injected before execve.

import Darwin
import Foundation

@MainActor
final class PtySupervisor {
    // MARK: Types

    enum SupervisorEvent: Sendable {
        case bytes(tabID: Int64, data: Data)
        case tabExited(tabID: Int64, status: Int32)
    }

    enum PtyError: Error, CustomStringConvertible {
        case forkpty(errno: Int32)
        case ttySize(errno: Int32)
        case duplicateTab(Int64)
        case notFound(Int64)
        case writeFailed(tabID: Int64, errno: Int32)

        var description: String {
            switch self {
            case .forkpty(let e): return "forkpty failed: \(strerrorString(e))"
            case .ttySize(let e): return "TIOCSWINSZ failed: \(strerrorString(e))"
            case .duplicateTab(let id): return "tab \(id) already has a live pty"
            case .notFound(let id): return "no pty for tab \(id)"
            case .writeFailed(let id, let e):
                return "write to pty tab \(id) failed: \(strerrorString(e))"
            }
        }
    }

    private struct Session {
        let masterFD: Int32
        let childPID: pid_t
        let source: DispatchSourceRead
        /// Drains `InternalEvent`s from the read source's background
        /// queue onto the main actor. Cancelling cancels iteration
        /// (the continuation finishes naturally on EOF / error).
        let drainTask: Task<Void, Never>
        /// Sendable handle that the read-source closure pushes
        /// events onto. Owned by the session so a teardown can
        /// `.finish()` it deterministically.
        let signalContinuation: AsyncStream<InternalEvent>.Continuation
    }

    /// Sendable bridge between the DispatchSourceRead's background
    /// queue and the `@MainActor` drain task. We can't capture
    /// `self` (a `@MainActor` class) into the source closure
    /// without Swift 6's runtime isolation check firing
    /// `dispatch_assert_queue(main)` on the read queue — see the
    /// stack trace in the M4c-validation crash report (`bug_type:
    /// 309`, queue `ai.stridelabs.Roost.pty.tab-N`). Routing every
    /// event through this value-type stream means the source
    /// closure only captures `Sendable` values (the continuation,
    /// `tabID`, `masterFD`), avoiding the actor capture.
    private enum InternalEvent: Sendable {
        case bytes(Data)
        case eof
        case readError(Int32)
        /// Yielded by `teardown` after its background reap loop
        /// finishes. The drain task uses this to emit `.tabExited`
        /// with the actual exit status (a `.eof`-driven path would
        /// instead go through `reapAndCleanup`, which is a no-op if
        /// the session was already removed from the map).
        case forcedExit(Int32)
    }

    private var sessions: [Int64: Session] = [:]
    private var pending: Set<Int64> = []
    private var observers: [UUID: @Sendable (SupervisorEvent) -> Void] = [:]

    // MARK: Subscribe

    @discardableResult
    func subscribe(_ handler: @escaping @Sendable (SupervisorEvent) -> Void) -> UUID {
        let token = UUID()
        observers[token] = handler
        return token
    }

    func unsubscribe(token: UUID) {
        observers.removeValue(forKey: token)
    }

    private func emit(_ event: SupervisorEvent) {
        for handler in observers.values {
            handler(event)
        }
    }

    // MARK: Spawn

    /// Spawn `argv` (empty → `$SHELL` or `/bin/sh`) at `cwd` with
    /// a freshly-allocated PTY of size `cols × rows`.
    /// `ROOST_TAB_ID` and `ROOST_SOCKET` are injected into the
    /// child's environment.
    ///
    /// Returns once `forkpty` has returned in the parent (the
    /// child's `execve` is in flight). Subscribers begin
    /// receiving `bytes` and (eventually) `tabExited` events for
    /// `tabID` immediately.
    ///
    /// Throws `duplicateTab` if `tabID` already has a live PTY
    /// (caller must `close()` first).
    func spawn(
        tabID: Int64,
        cwd: String,
        argv: [String],
        cols: UInt16,
        rows: UInt16,
        socketPath: String
    ) throws {
        if sessions[tabID] != nil || pending.contains(tabID) {
            throw PtyError.duplicateTab(tabID)
        }
        pending.insert(tabID)
        defer {
            // If we never promoted to sessions, drop the pending
            // marker so a retry can succeed.
            if sessions[tabID] == nil {
                pending.remove(tabID)
            }
        }

        // Build winsize.
        var winsize = Darwin.winsize(
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0
        )

        // Build argv + envp BEFORE forkpty. Anything that allocates
        // memory after fork is unsafe — libmalloc on macOS can
        // hold a lock during fork that the single-threaded child
        // never gets to release, deadlocking the next allocation.
        // The classic POSIX rule: only async-signal-safe functions
        // after fork. `strdup` and `Dictionary` traversal here run
        // in the parent, which is multithreaded-safe; the child
        // then only calls `chdir` / `execve` (both safe).
        //
        // If the caller passed an empty cwd (or one we can't
        // resolve), fall back to $HOME. Otherwise the child
        // inherits whatever process cwd Roost.app was launched
        // with — `/` for Finder-launched bundles, or the dev's
        // checkout root for `swift run`. Both are user-hostile
        // defaults; tabs should open in the user's home like the
        // pre-refactor Go binary and like a fresh interactive
        // shell.
        let resolvedCwd: String
        if cwd.isEmpty {
            resolvedCwd = ProcessInfo.processInfo.environment["HOME"] ?? ""
        } else {
            resolvedCwd = cwd
        }
        let cwdCopy = strdup(resolvedCwd)
        let cArgv = buildArgv(argv: argv)
        let cEnv = buildEnv(tabID: tabID, socketPath: socketPath)

        // forkpty allocates a PTY, forks, and dup2()s the slave
        // onto stdin/stdout/stderr in the child. We supply the
        // winsize and ignore the slave name; we never need it.
        var masterFD: Int32 = -1
        let pid = forkpty(&masterFD, nil, nil, &winsize)
        if pid < 0 {
            freeNullTerminated(cArgv)
            freeNullTerminated(cEnv)
            free(cwdCopy)
            throw PtyError.forkpty(errno: errno)
        }

        if pid == 0 {
            // CHILD: chdir + execve. ONLY async-signal-safe
            // calls here. No Swift String / Dictionary / new
            // allocations until execve replaces the image.
            // The child inherited the parent's COW pages, so
            // `cwdCopy` / `cArgv` / `cEnv` are valid pointers
            // into the child's own address space. We don't free
            // them in the child — execve replaces the whole
            // image including the heap.
            if let cwdCopy = cwdCopy, cwdCopy.pointee != 0 {
                _ = Darwin.chdir(cwdCopy)
            }
            // argv[0] is the program path. execve(2) signature:
            // execve(const char *path, char *const argv[], char *const envp[]).
            execve(cArgv[0], cArgv, cEnv)
            // execve failed; exit with 127 (conventional
            // "command not found").
            _exit(127)
        }

        // PARENT: free our copies of argv/env/cwd. The child has
        // its own COW pages so the parent's free here doesn't
        // affect it — and execve replaces the child's image
        // anyway shortly. CR-flagged leak on PR #78.
        freeNullTerminated(cArgv)
        freeNullTerminated(cEnv)
        free(cwdCopy)

        // PARENT: install the read source on a background queue,
        // bridge each read result to the main actor through a
        // `Sendable` AsyncStream. See `InternalEvent`'s doc comment
        // for why we can't capture `self` into the source closure
        // directly under Swift 6 strict concurrency.
        let queue = DispatchQueue(
            label: "ai.stridelabs.Roost.pty.tab-\(tabID)",
            qos: .userInteractive
        )
        let source = DispatchSource.makeReadSource(fileDescriptor: masterFD, queue: queue)
        let (signalStream, signalCont) = AsyncStream<InternalEvent>.makeStream()

        // Install the event handler via a `nonisolated` static
        // helper. Critical: defining the closure literal here
        // (inside this `@MainActor` method) makes Swift infer
        // `@MainActor` isolation on the closure even though
        // `setEventHandler`'s parameter is `@convention(block)`.
        // That inferred isolation later trips
        // `dispatch_assert_queue(main)` when the closure runs on
        // the Dispatch worker thread. Defining the closure inside
        // a `nonisolated static` method instead breaks the
        // inheritance chain.
        PtySupervisor.installReadHandler(
            source: source,
            masterFD: masterFD,
            signalCont: signalCont
        )

        // Drain on the main actor. `Task { @MainActor in ... }` is
        // constructed here in `spawn` (which is itself `@MainActor`),
        // so the capture of `self` happens on the right actor — no
        // boundary-crossing runtime check, and the iteration body
        // runs on main where `emit` and `reapAndCleanup` belong.
        let drainTask = Task { @MainActor [weak self] in
            // `emittedExit` guards against double `.tabExited`
            // emission when both an EOF and a teardown-initiated
            // forced exit race onto the stream. AsyncStream
            // guarantees FIFO delivery of yields, so the drain
            // can simply remember whether it already saw a
            // terminal event. Without this guard, sub-agent
            // review of M6-M9 flagged the scenario where the
            // read source yields `.eof`, the drain processes it
            // (reapAndCleanup emits `.tabExited(status: 0)`),
            // and a subsequent teardown's bg-reap yield of
            // `.forcedExit(-1)` would emit a second
            // `.tabExited(-1)`.
            var emittedExit = false
            for await event in signalStream {
                guard let self else { return }
                switch event {
                case .bytes(let data):
                    self.emit(.bytes(tabID: tabID, data: data))
                case .eof, .readError:
                    // reapAndCleanup returns true iff it emitted
                    // `.tabExited` (false when the session was
                    // already removed by a racing close()).
                    if self.reapAndCleanup(tabID: tabID, expectedPID: pid) {
                        emittedExit = true
                    }
                case .forcedExit(let status):
                    // teardown() already removed the session
                    // from the map and reaped the child; emit so
                    // subscribers see the close — but only if a
                    // racing EOF didn't already emit.
                    if !emittedExit {
                        self.emit(.tabExited(tabID: tabID, status: status))
                        emittedExit = true
                    }
                }
            }
        }

        let session = Session(
            masterFD: masterFD,
            childPID: pid,
            source: source,
            drainTask: drainTask,
            signalContinuation: signalCont
        )
        sessions[tabID] = session
        pending.remove(tabID)
        source.resume()
    }

    /// Write `data` to the tab's PTY. Caller is on the main
    /// actor; the actual `write(2)` is short-blocking but the
    /// dispatch through this method preserves ordering.
    ///
    /// Loops on partial writes + retries on `EINTR`. Throws on
    /// any other negative `write(2)` return (the previous
    /// single-call version returned -1 as a signed byte count
    /// and hid IO errors, and dropped tail bytes on partial
    /// writes — both CR-flagged).
    @discardableResult
    func write(tabID: Int64, data: Data) throws -> Int {
        guard let session = sessions[tabID] else {
            throw PtyError.notFound(tabID)
        }
        if data.isEmpty { return 0 }
        let masterFD = session.masterFD
        let total = data.count
        let written: Int = try data.withUnsafeBytes { raw -> Int in
            guard let base = raw.baseAddress else { return 0 }
            var offset = 0
            while offset < total {
                let remaining = total - offset
                let n = Darwin.write(masterFD, base.advanced(by: offset), remaining)
                if n < 0 {
                    if errno == EINTR { continue }
                    throw PtyError.writeFailed(tabID: tabID, errno: errno)
                }
                if n == 0 {
                    // 0 from write() on a PTY master fd is
                    // unusual; treat as a writer disconnect
                    // (peer closed slave) rather than spinning.
                    break
                }
                offset += n
            }
            return offset
        }
        return written
    }

    func resize(tabID: Int64, cols: UInt16, rows: UInt16) throws {
        guard let session = sessions[tabID] else {
            throw PtyError.notFound(tabID)
        }
        var ws = Darwin.winsize(
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0
        )
        let rc = ioctl(session.masterFD, TIOCSWINSZ, &ws)
        if rc < 0 {
            throw PtyError.ttySize(errno: errno)
        }
    }

    /// Close a tab's PTY. Cancels the read source, SIGHUPs the
    /// child, then `waitpid(WNOHANG)` loop until reap (or
    /// SIGKILL fallback after a brief timeout). Final
    /// `tabExited` event fires from this path if it didn't
    /// already fire from EOF.
    func close(tabID: Int64) {
        guard let session = sessions.removeValue(forKey: tabID) else { return }
        teardown(session: session, tabID: tabID)
    }

    /// Quit-time reap: close every live session, SIGHUP all
    /// children, waitpid loop with timeout, SIGKILL fallback.
    /// Used on `applicationWillTerminate`.
    func quitAll() {
        let live = sessions
        sessions.removeAll()
        for (tabID, session) in live {
            teardown(session: session, tabID: tabID)
        }
    }

    func has(_ tabID: Int64) -> Bool {
        sessions[tabID] != nil
    }

    // MARK: Read-source helpers

    /// Install the DispatchSourceRead's event handler. Declared
    /// `nonisolated static` so the closure literal inside doesn't
    /// inherit `@MainActor` isolation from the enclosing call site
    /// — see the doc comment in `spawn(...)` for the
    /// `dispatch_assert_queue(main)` crash that motivated this
    /// extraction.
    nonisolated private static func installReadHandler(
        source: DispatchSourceRead,
        masterFD: Int32,
        signalCont: AsyncStream<InternalEvent>.Continuation
    ) {
        source.setEventHandler {
            Self.handleReadSourceEvent(
                masterFD: masterFD,
                signalCont: signalCont
            )
        }
    }

    /// Nonisolated helper that the DispatchSourceRead event handler
    /// trampolines into. Reads up to 4 KiB from `masterFD` and
    /// yields the result onto the InternalEvent stream the drain
    /// task is iterating.
    nonisolated private static func handleReadSourceEvent(
        masterFD: Int32,
        signalCont: AsyncStream<InternalEvent>.Continuation
    ) {
        let cap = 4096
        var buf = [UInt8](repeating: 0, count: cap)
        let n = buf.withUnsafeMutableBufferPointer { ptr -> Int in
            Darwin.read(masterFD, ptr.baseAddress, cap)
        }
        if n > 0 {
            signalCont.yield(.bytes(Data(buf.prefix(n))))
        } else if n == 0 {
            // EOF — child closed the slave. Drain task will call
            // reapAndCleanup on receipt of `.eof`.
            signalCont.yield(.eof)
            signalCont.finish()
        } else {
            // n < 0: EAGAIN / EWOULDBLOCK / EINTR are transient
            // (the source re-fires on the next readable
            // notification). Anything else is terminal.
            switch errno {
            case EAGAIN, EWOULDBLOCK, EINTR:
                break
            default:
                signalCont.yield(.readError(errno))
                signalCont.finish()
            }
        }
    }

    // MARK: Internal

    /// Returns true iff this call actually emitted `.tabExited`
    /// (and therefore the caller should consider the exit signal
    /// "delivered" for double-emit suppression).
    @discardableResult
    private func reapAndCleanup(tabID: Int64, expectedPID: pid_t) -> Bool {
        guard let session = sessions[tabID], session.childPID == expectedPID else {
            return false
        }
        sessions.removeValue(forKey: tabID)
        let status = reapChild(pid: expectedPID)
        Darwin.close(session.masterFD)
        emit(.tabExited(tabID: tabID, status: status))
        return true
    }

    private func teardown(session: Session, tabID: Int64) {
        // The blocking reap loop (waitpid + usleep) used to run
        // inline on `@MainActor`, which would freeze the AppKit
        // main loop for up to ~200ms (or longer if a SIGKILL
        // fallback is needed). Move it to a background DispatchQueue
        // and hop back to the main actor to emit `tabExited`.
        //
        // The background block does NOT capture `self` — instead it
        // yields the exit signal onto the same `signalContinuation`
        // the read source uses, and the existing drain task
        // (`@MainActor`-isolated) calls `emit(.tabExited(...))`.
        // Same rationale as the read source: avoid Swift 6's
        // `dispatch_assert_queue(main)` runtime check that fires
        // when a `@MainActor` reference is captured into a
        // non-isolated dispatch closure.
        session.source.cancel()
        let masterFD = session.masterFD
        let childPID = session.childPID
        let signalCont = session.signalContinuation
        let drainTask = session.drainTask
        DispatchQueue.global(qos: .userInitiated).async {
            kill(childPID, SIGHUP)
            var status: Int32 = 0
            var reaped = false
            for _ in 0..<20 {
                let rc = waitpid(childPID, &status, WNOHANG)
                if rc == childPID {
                    reaped = true
                    break
                }
                if rc < 0 && errno == ECHILD {
                    reaped = true
                    break
                }
                usleep(10_000)
            }
            if !reaped {
                kill(childPID, SIGKILL)
                waitpid(childPID, &status, 0)
            }
            Darwin.close(masterFD)
            // Signal the drain task to emit .tabExited with the
            // real status before letting the stream end.
            signalCont.yield(.forcedExit(exitStatus(status)))
            signalCont.finish()
            _ = drainTask
        }
    }

    /// Build the NULL-terminated argv array. Empty input falls
    /// back to `$SHELL` or `/bin/sh`. Returned strings are
    /// strdup'd; the parent leaks them at spawn-scope (small
    /// constant overhead).
    private func buildArgv(argv: [String]) -> [UnsafeMutablePointer<CChar>?] {
        let resolved: [String]
        if argv.isEmpty {
            let shell = ProcessInfo.processInfo.environment["SHELL"] ?? "/bin/sh"
            resolved = [shell]
        } else {
            resolved = argv
        }
        var out: [UnsafeMutablePointer<CChar>?] = resolved.map { strdup($0) }
        out.append(nil)
        return out
    }

    /// Build the NULL-terminated envp array. Inherits the
    /// parent's environment then overlays Roost's injected vars.
    private func buildEnv(tabID: Int64, socketPath: String) -> [UnsafeMutablePointer<CChar>?] {
        var env: [String: String] = ProcessInfo.processInfo.environment
        env["TERM"] = env["TERM"] ?? "xterm-256color"
        env["COLORTERM"] = "truecolor"
        env["ROOST_TAB_ID"] = String(tabID)
        env["ROOST_SOCKET"] = socketPath
        var out: [UnsafeMutablePointer<CChar>?] = env.map { strdup("\($0)=\($1)") }
        out.append(nil)
        return out
    }
}

// MARK: - Helpers

private func reapChild(pid: pid_t) -> Int32 {
    var status: Int32 = 0
    let rc = waitpid(pid, &status, 0)
    if rc < 0 {
        return -1
    }
    return exitStatus(status)
}

private func exitStatus(_ raw: Int32) -> Int32 {
    // POSIX `WIFEXITED` / `WEXITSTATUS` aren't bridged into
    // Darwin module on every release; do the bit math directly.
    // status layout: low 7 bits = signal, bit 7 = core dump,
    // bits 8-15 = exit code.
    if raw & 0x7f == 0 {
        return (raw >> 8) & 0xff
    }
    // Signal-terminated: surface as -<signal> so callers can
    // distinguish from a normal non-zero exit.
    return -((raw) & 0x7f)
}

private func strerrorString(_ code: Int32) -> String {
    if let c = strerror(code), let s = String(validatingUTF8: c) {
        return s
    }
    return "errno \(code)"
}

/// Free each `strdup`'d entry in a NULL-terminated argv/env
/// array. The terminator nil itself isn't freed — it's just an
/// `Optional<UnsafeMutablePointer<CChar>>` sentinel.
private func freeNullTerminated(_ buf: [UnsafeMutablePointer<CChar>?]) {
    for ptr in buf {
        if let ptr = ptr { free(ptr) }
    }
}
