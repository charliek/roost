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
//   * Exit: `SIGCHLD` + `waitpid(WNOHANG)`. On exit, cancel the
//     read source (its cancel handler closes the master), fire
//     `tabExited` on the supervisor's event sink.
//   * Quit-time reap: iterate all sessions, `SIGHUP` →
//     `waitpid` with timeout → `SIGKILL` fallback. No zombies.
//   * Env: `ROOST_TAB_ID` + `ROOST_SOCKET` + `TERM` +
//     `COLORTERM=truecolor` + `FORCE_HYPERLINK=1` injected before
//     execve.

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
        /// Open while the session is in `sessions`: the only close is
        /// `source`'s cancel handler, and the source is cancelled only
        /// once the session has left the map.
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
    ///
    /// `cwd` is used as given: the caller resolves it
    /// (`LocalClient.spawnCwd`). A child that cannot enter it — an
    /// unenterable directory, a race, an empty `cwd` — exits 126
    /// before `execve` instead of running on in Roost.app's own launch
    /// directory, so the tab ends through the ordinary exit path, as a
    /// Rust spawn whose `chdir` fails ends its tab.
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
        // then only calls `chdir` / `execve` / `_exit` (all safe).
        let cwdCopy = strdup(cwd)
        let cArgv = buildArgv(argv: argv)
        let cEnv = buildEnv(tabID: tabID, socketPath: socketPath, argv: argv)

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
            if Darwin.chdir(cwdCopy) != 0 {
                _exit(126)
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

    /// Best-effort native read of the cwd of the tab's foreground job:
    /// the first of `nativeCwds`. Nil if the tab has no live PTY or
    /// every read fails.
    func foregroundCwd(tabID: Int64) -> String? {
        nativeCwds(tabID: tabID).first
    }

    /// Best-effort native reads of the tab's cwd, best first — what a
    /// new tab opened from it inherits, since a new tab spawns a LOCAL
    /// shell. Rust's `PtySupervisor::native_cwds`: first the cwd of the
    /// foreground process group's leader, the job the shell is running
    /// (a nested shell, a command); then the direct child's, the shell
    /// Roost started, which is also the leader while the shell sits at
    /// its prompt. Empty if the tab has no live PTY or every read fails.
    ///
    /// A leader that changes directory moves the answer with it — `git`
    /// under its pager reads as the repository root. Intended: that is
    /// where the job is.
    ///
    /// Where Rust holds the child's reap latch, the main actor does the
    /// same here: a child is reaped only after its session has left
    /// `sessions`, so while it is in the map its pid names this child and
    /// its master fd is open.
    func nativeCwds(tabID: Int64) -> [String] {
        guard let session = sessions[tabID] else { return [] }
        return [
            leaderCwd(masterFD: session.masterFD, child: session.childPID),
            processCwd(pid: session.childPID),
        ].compactMap { $0 }
    }

    /// The tab's read source, for the #557 seam test only.
    func readSourceForTesting(tabID: Int64) -> DispatchSourceRead? {
        sessions[tabID]?.source
    }

    // MARK: Read-source helpers

    /// Install the DispatchSourceRead's event and cancel handlers.
    /// Declared `nonisolated static` so the closure literals inside
    /// don't inherit `@MainActor` isolation from the enclosing call
    /// site — see the doc comment in `spawn(...)` for the
    /// `dispatch_assert_queue(main)` crash that motivated this
    /// extraction.
    ///
    /// The cancel handler is the one place the master fd is closed.
    /// Dispatch runs it only after any event handler in flight has
    /// returned, and never runs the event handler again, so no read
    /// can reach a later descriptor given the same number (#557).
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
        source.setCancelHandler {
            Darwin.close(masterFD)
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
        // Before the reap: a source at EOF fires until it is cancelled.
        session.source.cancel()
        let status = reapChild(pid: expectedPID)
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
            // Signal the drain task to emit .tabExited with the
            // real status before letting the stream end.
            signalCont.yield(.forcedExit(exitStatus(status)))
            signalCont.finish()
            _ = drainTask
        }
    }

    /// Build the NULL-terminated argv array. Empty input falls back
    /// to the user's `$SHELL` (or `/bin/sh`) launched as a LOGIN
    /// shell — see `loginShellArgv`. Returned strings are strdup'd;
    /// the parent leaks them at spawn-scope (small constant overhead).
    private func buildArgv(argv: [String]) -> [UnsafeMutablePointer<CChar>?] {
        let shell = ProcessInfo.processInfo.environment["SHELL"] ?? "/bin/sh"
        let resolved = loginShellArgv(argv, shell: shell)
        // Modern bash: add `--posix` so it honors ENV (the only
        // per-interactive-shell hook), which Roost points at roost.bash —
        // see `shouldBashBootstrap` and `buildEnv`. The argv `--posix` and
        // the env injection must agree, so both gate on the same check.
        let resourcesDir = Bundle.roostResources.bundleURL.path
        let execArgv = withBashPosix(
            resolved, apply: shouldBashBootstrap(resolved, resourcesDir: resourcesDir))
        var out: [UnsafeMutablePointer<CChar>?] = execArgv.map { strdup($0) }
        out.append(nil)
        return out
    }

    /// Build the NULL-terminated envp array. Inherits the
    /// parent's environment then overlays Roost's injected vars.
    private func buildEnv(tabID: Int64, socketPath: String, argv: [String])
        -> [UnsafeMutablePointer<CChar>?]
    {
        let env = childEnvironment(
            base: ProcessInfo.processInfo.environment,
            tabID: tabID,
            socketPath: socketPath,
            argv: argv,
            resourcesDir: Bundle.roostResources.bundleURL.path,
            version: (Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String)
                ?? "dev",
            agentHook: bundledRoostctl()
        )
        var out: [UnsafeMutablePointer<CChar>?] = env.map { strdup("\($0)=\($1)") }
        out.append(nil)
        return out
    }
}

/// Whether to bash-auto-bootstrap `resolvedArgv`: the pure predicate
/// (`bashAutobootstrap`) AND the shipped roost.bash being present at
/// `resourcesDir`. `--posix` and the ENV injection must be applied
/// together — a `--posix` shell with no ENV script to source would be
/// stuck in POSIX mode with no startup recreation — so `buildArgv` and
/// `buildEnv` both gate on this.
func shouldBashBootstrap(_ resolvedArgv: [String], resourcesDir: String) -> Bool {
    bashAutobootstrap(resolvedArgv, isDarwin: true)
        && FileManager.default.fileExists(
            atPath: resourcesDir + "/shell-integration/roost.bash")
}

/// The environment a Roost tab's child process sees: `base` (the parent's
/// environment) overlaid with Roost's injected vars. Pure so tests can
/// assert the contract without spawning a PTY.
/// Roost's bundled `roostctl` (`Roost.app/Contents/Resources/bin/roostctl`),
/// handed to providers as `ROOST_ROOSTCTL` and to every spawned tab as
/// `ROOST_AGENT_HOOK`. `nil` when not present (e.g. a `swift run` dev build
/// with no embedded CLI) — the provider then falls back to a PATH
/// `roostctl`, and the tab simply doesn't get the variable.
func bundledRoostctl() -> String? {
    guard let url = Bundle.main.resourceURL?.appendingPathComponent("bin/roostctl"),
        FileManager.default.isExecutableFile(atPath: url.path)
    else { return nil }
    return url.path
}

func childEnvironment(
    base: [String: String],
    tabID: Int64,
    socketPath: String,
    argv: [String],
    resourcesDir: String,
    version: String,
    agentHook: String?
) -> [String: String] {
    var env = base
    // Advertise the terminal Roost provides — force TERM rather than
    // inheriting the launching terminal's (a child seeing an inherited
    // TERM=tmux-256color / xterm-kitty would emit unsupported seqs).
    env["TERM"] = "xterm-256color"
    env["COLORTERM"] = "truecolor"
    // Forcing TERM makes an inherited TERMINFO wrong: it points at the
    // launching terminal's private DB (e.g. Ghostty's, which has no
    // xterm-256color entry), so strict $TERMINFO readers would find no
    // entry for the TERM Roost advertises.
    env.removeValue(forKey: "TERMINFO")
    // Advertise OSC 8 hyperlink support. Roost renders + opens OSC 8
    // links (Cmd-click), but the `supports-hyperlinks` library many
    // CLIs gate on — Claude Code, anything on chalk/terminal-link —
    // only allowlists known terminals by TERM_PROGRAM, and "Roost"
    // isn't one. Without this they emit plain text instead of a link
    // (e.g. Claude Code's footer "PR #N"). FORCE_HYPERLINK is that
    // ecosystem's "my terminal supports it" override; honest here
    // because we genuinely do.
    env["FORCE_HYPERLINK"] = "1"
    env["ROOST_TAB_ID"] = String(tabID)
    env["ROOST_SOCKET"] = socketPath
    // The one hook entrypoint every installed agent config invokes
    // (plan 046 §3.2). Indirecting through the environment is what keeps
    // the written config identical on every machine and on every host —
    // it names no path of Roost's. Omitted rather than guessed when the
    // bundled CLI is absent: the installed command's fallback branch
    // reads an *unset* variable as "not inside Roost", where a path that
    // does not exist would exec-fail with no JSON on stdout. An
    // inherited value is dropped rather than passed through — it must
    // always describe *this* Roost, never an outer one.
    if let agentHook {
        env["ROOST_AGENT_HOOK"] = agentHook
    } else {
        env.removeValue(forKey: "ROOST_AGENT_HOOK")
    }
    // Roost shell-integration contract: identify the terminal and
    // point shells at the shipped scripts (under
    // $ROOST_RESOURCES_DIR/shell-integration). TERM stays
    // xterm-256color (above) — we don't masquerade as another
    // terminal. ROOST_SHELL_FEATURES is user-overridable.
    env["TERM_PROGRAM"] = "Roost"
    env["TERM_PROGRAM_VERSION"] = version
    env["ROOST_SHELL_INTEGRATION"] = "1"
    env["ROOST_SHELL_FEATURES"] = env["ROOST_SHELL_FEATURES"] ?? "cwd,title,marks,prompt,ssh-env"
    env["ROOST_RESOURCES_DIR"] = resourcesDir
    // Auto-bootstrap the shipped integration with no rc edit. Resolve the
    // same argv `buildArgv` execs (default case → `[$SHELL, -l]`), then:
    //   * zsh: point ZDOTDIR at our shim — it restores the user's
    //     ZDOTDIR, runs their real zsh startup, then loads roost.zsh.
    //   * modern bash: set ENV + ROOST_BASH_INJECT so the `--posix` shell
    //     sources roost.bash, which recreates startup then loads the
    //     integration. Apple's bash 3.2 can't (skipped in the helper).
    let resolvedArgv = loginShellArgv(argv, shell: env["SHELL"] ?? "/bin/sh")
    let shellName = resolvedArgv.first.map { ($0 as NSString).lastPathComponent } ?? ""
    if shellName == "zsh" {
        if let userZdotdir = env["ZDOTDIR"], !userZdotdir.isEmpty {
            env["ROOST_ZSH_ZDOTDIR"] = userZdotdir
        }
        env["ZDOTDIR"] = resourcesDir + "/shell-integration/zsh"
    } else if shouldBashBootstrap(resolvedArgv, resourcesDir: resourcesDir) {
        for (key, value) in bashBootstrapEnv(
            resourcesDir: resourcesDir,
            existingEnv: env["ENV"],
            existingHistfile: env["HISTFILE"],
            home: env["HOME"]
        ) {
            env[key] = value
        }
    }
    return env
}

// MARK: - Helpers

/// Resolve the argv to exec. An empty argv (the plain "open a shell"
/// case) becomes the user's `$SHELL` (or `/bin/sh`) launched as a
/// LOGIN shell via `-l`, so it sources `.bash_profile` / `.zprofile`:
/// that silences macOS's bash deprecation banner and puts login-only
/// PATH entries (e.g. `claude`) in scope, matching Terminal.app /
/// Ghostty. A non-empty argv (launcher commands) is passed through
/// verbatim — we never force `-l` onto an explicit command line.
func loginShellArgv(_ argv: [String], shell: String) -> [String] {
    argv.isEmpty ? [shell, "-l"] : argv
}

/// Whether to auto-bootstrap a modern bash — i.e. add `--posix` and point
/// ENV at roost.bash so the integration loads with no rc edit (see
/// `bashBootstrapEnv` and roost.bash's inject header). True iff `argv[0]`
/// is a `bash`, it isn't Apple's `/bin/bash` (3.2, SIP-locked — its ENV
/// POSIX path is patched out, so we leave it for the documented manual
/// source), and the only extra args are plain login/interactive flags
/// (`-l`/`-i`). That admits the default-shell case (`[$SHELL, -l]`) and an
/// explicit `[bash, -l]`, but passes launcher commands (`-c`, `--norc`,
/// `--rcfile`, …) and an already-`--posix` argv through untouched — adding
/// `--posix` to those would change their semantics.
func bashAutobootstrap(_ argv: [String], isDarwin: Bool) -> Bool {
    guard let arg0 = argv.first else { return false }
    guard (arg0 as NSString).lastPathComponent == "bash" else { return false }
    if isDarwin && arg0 == "/bin/bash" { return false }
    return argv.dropFirst().allSatisfy { $0 == "-l" || $0 == "-i" }
}

/// Insert `--posix` where bash needs it — right after argv[0], before the
/// short `-l`/`-i` flags. bash rejects a GNU long option that follows a
/// short one (`bash -l --posix` errors with `--: invalid option`), so the
/// long option goes first. Returns `argv` unchanged when `apply` is false.
func withBashPosix(_ argv: [String], apply: Bool) -> [String] {
    guard apply else { return argv }
    var out = argv
    out.insert("--posix", at: 1)
    return out
}

/// The env vars to overlay when auto-bootstrapping bash (see roost.bash's
/// inject header). `existingEnv`/`existingHistfile` are the child's
/// inherited values, if any. ENV points bash at roost.bash;
/// ROOST_BASH_INJECT="1" tells it to recreate startup (and marks an
/// auto-load vs. a manual source). A prior ENV is preserved into
/// ROOST_BASH_ENV so the shim can restore it. HISTFILE is pinned to
/// ~/.bash_history (POSIX mode would default it to ~/.sh_history) only when
/// fully unset, with ROOST_BASH_UNEXPORT_HISTFILE telling the shim to
/// un-export it afterward. An *empty* HISTFILE is left alone — that's the
/// idiom for disabling history, so we must not re-enable it (matches
/// Ghostty's null-only check).
func bashBootstrapEnv(
    resourcesDir: String,
    existingEnv: String?,
    existingHistfile: String?,
    home: String?
) -> [String: String] {
    var out: [String: String] = [:]
    if let prev = existingEnv, !prev.isEmpty {
        out["ROOST_BASH_ENV"] = prev
    }
    out["ENV"] = resourcesDir + "/shell-integration/roost.bash"
    out["ROOST_BASH_INJECT"] = "1"
    if existingHistfile == nil, let home, !home.isEmpty {
        out["HISTFILE"] = home + "/.bash_history"
        out["ROOST_BASH_UNEXPORT_HISTFILE"] = "1"
    }
    return out
}

/// Who a process is, as far as the foreground-leader read needs it —
/// Rust's `ProcStamp` in `crates/roost-engine/src/pty.rs`.
struct ProcStamp {
    var pid: pid_t
    var pgid: pid_t
    var session: pid_t
    var tty: UInt32
    var startSec: UInt64
    var startUsec: UInt64
}

/// Whether the stamps taken before and after a leader's cwd read are one
/// process, leading its own group, in the child's session and on the
/// child's terminal — Rust's `same_leader`. The start time is what tells
/// a reused pid apart.
func sameLeader(_ before: ProcStamp, _ after: ProcStamp, childSID: pid_t, childTTY: UInt32) -> Bool {
    [before, after].allSatisfy { stamp in
        stamp.pgid == stamp.pid && stamp.session == childSID && stamp.tty == childTTY
    } && before.pid == after.pid
        && (before.startSec, before.startUsec) == (after.startSec, after.startUsec)
}

/// `pid`'s stamp via `proc_pidinfo` (`PROC_PIDTBSDINFO`), or nil.
private func procStamp(pid: pid_t) -> ProcStamp? {
    var info = proc_bsdinfo()
    let size = Int32(MemoryLayout<proc_bsdinfo>.size)
    guard proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, &info, size) == size else { return nil }
    // `proc_bsdinfo` carries no session id.
    let session = getsid(pid)
    guard session >= 0 else { return nil }
    return ProcStamp(
        pid: pid_t(bitPattern: info.pbi_pid),
        pgid: pid_t(bitPattern: info.pbi_pgid),
        session: session,
        tty: info.e_tdev,
        startSec: info.pbi_start_tvsec,
        startUsec: info.pbi_start_tvusec
    )
}

/// The cwd of the leader of the terminal's foreground process group,
/// when that is not `child` itself: a job the shell is running, or a
/// nested shell — Rust's `leader_cwd`.
///
/// `forkpty` makes the child a session leader with the pty as its
/// controlling terminal, so the master's foreground group is the job's.
/// The leader is not our child, though, and nothing holds its pid, so its
/// cwd is read between two `ProcStamp`s and kept only when `sameLeader`
/// matches them. Asking the terminal for its foreground group again
/// would not do: a dead job's group stays the foreground one until the
/// shell calls `tcsetpgrp`, and its pid can be reused meanwhile. A leader
/// this user can't read (`sudo -s`) is nil, like any failed read.
private func leaderCwd(masterFD: Int32, child: pid_t) -> String? {
    let leader = tcgetpgrp(masterFD)
    guard leader > 0, leader != child,
        let own = procStamp(pid: child),
        let before = procStamp(pid: leader),
        let cwd = processCwd(pid: leader),
        let after = procStamp(pid: leader),
        sameLeader(before, after, childSID: child, childTTY: own.tty)
    else { return nil }
    return cwd
}

/// The current working directory of `pid` via `proc_pidinfo`
/// (`PROC_PIDVNODEPATHINFO`). Returns nil on any failure.
private func processCwd(pid: pid_t) -> String? {
    var info = proc_vnodepathinfo()
    let size = Int32(MemoryLayout<proc_vnodepathinfo>.size)
    let rc = proc_pidinfo(pid, PROC_PIDVNODEPATHINFO, 0, &info, size)
    guard rc == size else { return nil }
    let path = withUnsafeBytes(of: &info.pvi_cdir.vip_path) { raw -> String in
        guard let base = raw.baseAddress else { return "" }
        return String(cString: base.assumingMemoryBound(to: CChar.self))
    }
    return path.isEmpty ? nil : path
}

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
