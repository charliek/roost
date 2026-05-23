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
        let cwdCopy = strdup(cwd)
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
        // hop to main for every chunk + exit.
        let queue = DispatchQueue(
            label: "ai.stridelabs.Roost.pty.tab-\(tabID)",
            qos: .userInteractive
        )
        let source = DispatchSource.makeReadSource(fileDescriptor: masterFD, queue: queue)
        let session = Session(masterFD: masterFD, childPID: pid, source: source)
        sessions[tabID] = session
        pending.remove(tabID)

        let supervisor = self
        source.setEventHandler {
            // Read up to 4 KiB; the source fires once per
            // readable-state notification, but the queue is
            // serial so back-to-back fires can drain.
            let cap = 4096
            var buf = [UInt8](repeating: 0, count: cap)
            let n = buf.withUnsafeMutableBufferPointer { ptr -> Int in
                Darwin.read(masterFD, ptr.baseAddress, cap)
            }
            if n > 0 {
                let chunk = Data(buf.prefix(n))
                Task { @MainActor in
                    supervisor.emit(.bytes(tabID: tabID, data: chunk))
                }
            } else if n == 0 {
                // EOF — child closed the slave. Reap below via
                // the cancel handler.
                Task { @MainActor in
                    supervisor.reapAndCleanup(tabID: tabID, expectedPID: pid)
                }
            } else {
                // n < 0: classify the errno. EAGAIN / EWOULDBLOCK
                // / EINTR are transient — the read source will
                // re-fire on the next readable notification.
                // Anything else (EBADF, EIO, etc.) is terminal:
                // reap the child and tear the session down.
                // CR-flagged on PR #78 — the old code only
                // checked EBADF.
                switch errno {
                case EAGAIN, EWOULDBLOCK, EINTR:
                    break
                default:
                    Task { @MainActor in
                        supervisor.reapAndCleanup(tabID: tabID, expectedPID: pid)
                    }
                }
            }
        }
        source.resume()
    }

    /// Write `data` to the tab's PTY. Caller is on the main
    /// actor; the actual `write(2)` is short-blocking but the
    /// dispatch through this method preserves ordering.
    /// Throws on negative `write(2)` return — CR-flagged that
    /// the prior version returned -1 as a signed byte count and
    /// hid IO errors.
    @discardableResult
    func write(tabID: Int64, data: Data) throws -> Int {
        guard let session = sessions[tabID] else {
            throw PtyError.notFound(tabID)
        }
        if data.isEmpty { return 0 }
        let masterFD = session.masterFD
        let result: Int = data.withUnsafeBytes { raw -> Int in
            guard let p = raw.baseAddress else { return 0 }
            return Darwin.write(masterFD, p, raw.count)
        }
        if result < 0 {
            throw PtyError.writeFailed(tabID: tabID, errno: errno)
        }
        return result
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

    // MARK: Internal

    private func reapAndCleanup(tabID: Int64, expectedPID: pid_t) {
        guard let session = sessions[tabID], session.childPID == expectedPID else {
            return
        }
        sessions.removeValue(forKey: tabID)
        let status = reapChild(pid: expectedPID)
        Darwin.close(session.masterFD)
        emit(.tabExited(tabID: tabID, status: status))
    }

    private func teardown(session: Session, tabID: Int64) {
        // The blocking reap loop (waitpid + usleep) used to run
        // inline on `@MainActor`, which would freeze the GTK/AppKit
        // main loop for up to ~200ms (or longer if a SIGKILL
        // fallback is needed). Move it to a background DispatchQueue
        // and hop back to the main actor to emit `tabExited`.
        // CR-flagged on PR #78.
        session.source.cancel()
        let masterFD = session.masterFD
        let childPID = session.childPID
        let supervisor = self
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
            let exit = exitStatus(status)
            Task { @MainActor in
                supervisor.emit(.tabExited(tabID: tabID, status: exit))
            }
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
