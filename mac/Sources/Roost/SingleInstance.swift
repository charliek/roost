// SingleInstance.swift — daemon-removal refactor M4c.
//
// Mac-side single-instance enforcement. Mirrors the Rust side in
// `crates/roost-engine/src/single_instance.rs` so the variants have
// the same observable behavior: first launch acquires an exclusive
// `flock(LOCK_EX | LOCK_NB)` and writes its PID; second launch fails
// the lock, reads the PID from the existing file, and exits 0. The
// existing window is activated by the IPC layer (M6 hardens that
// path); this file only owns lock acquisition + PID writing.
//
// TWO LOCKS, neither legacy (see `acquirePair`):
//   * the socket/bind lock at `<socket dir>/roost.lock` guards the
//     probe→unlink→bind sequence and the socket's lifetime;
//   * the state lock at `<state dir>/state.lock` guards `state.json`.
// They move independently — `ROOST_STATE_DIR` moves only the second —
// so one lock could never cover both. Acquisition order (socket, then
// state) and release order (the reverse) are load-bearing: mixed
// orders let two starting processes refuse each other.
//
// Why flock (BSD style) and not POSIX fcntl(F_SETLK):
//   * The GTK side uses `fs2::FileExt::try_lock_exclusive`, which
//     is a thin wrapper around flock(2). Using the same primitive
//     keeps the cross-platform behavior identical — both variants
//     fail in the same way when the lock is contended.
//   * POSIX record locks (fcntl F_SETLK) have lock-vs-fd semantics
//     that interact badly with multi-fd handling and "any close
//     drops all locks" surprises. flock is a 1:1 lock-per-fd model.
//
// Why @_silgen_name:
//   * The Swift importer brings in both the C function `flock(int,
//     int)` from <sys/file.h> AND the struct `flock` from
//     <sys/fcntl.h> (used as the lock-spec argument to fcntl
//     F_SETLK). When both are visible Swift picks the type lookup
//     and reports `flock(fd, LOCK_EX | LOCK_NB)` as "call that
//     takes no arguments" — because `struct flock`'s default init
//     takes no parameters. `@_silgen_name` lets us bind to the C
//     symbol directly, sidestepping the import collision. This
//     trick is widely used in the Swift ecosystem (swift-nio,
//     swift-system) for the same reason.

import Darwin
import Foundation

@_silgen_name("flock")
private func roost_flock(_ fd: Int32, _ op: Int32) -> Int32

final class SingleInstance: @unchecked Sendable {
    /// Outcome of an `acquire(...)` attempt. The caller decides what
    /// to do — typically `.acquired` continues startup, `.alreadyHeld`
    /// activates the existing window and exits 0, `.bypassed` skips
    /// enforcement for dev/test workflows.
    enum Status {
        case acquired(SingleInstance)
        case alreadyHeld(holderPID: pid_t)
        case bypassed
    }

    enum SingleInstanceError: Error, CustomStringConvertible {
        case openFailed(path: String, errno: Int32)
        case lockFailed(errno: Int32)
        case writeFailed(errno: Int32)
        /// The state lock could not be taken for a reason other than
        /// contention. Thrown — not returned — because the policy for
        /// this one lock is fail closed: `state.json` has no second
        /// guard, so running without it risks the exact corruption the
        /// lock exists to prevent.
        case stateLockUnavailable(path: String, underlying: any Error)

        var description: String {
            switch self {
            case .openFailed(let p, let e): return "open(\(p)) failed: \(strerrorString(e))"
            case .lockFailed(let e): return "flock failed: \(strerrorString(e))"
            case .writeFailed(let e): return "write(pid) failed: \(strerrorString(e))"
            case .stateLockUnavailable(let p, let underlying):
                return "state lock \(p) unavailable: \(underlying)"
            }
        }
    }

    /// Module-internal rather than private so `SingleInstanceTests`
    /// can hand the fd to a child process and prove the LOCK_UN in
    /// `deinit` (issue #324).
    let lockFD: Int32
    let lockPath: String

    private init(lockFD: Int32, lockPath: String) {
        self.lockFD = lockFD
        self.lockPath = lockPath
    }

    deinit {
        // LOCK_UN first, then close. Closing alone is not enough:
        // flock(2) locks belong to the open file description, so a
        // fork()ed child that inherited the fd — every PTY spawn, in
        // the window between fork and exec — keeps the lock alive past
        // our close. LOCK_UN clears it on the description all those
        // fds share. Same defect and same fix as the Rust side
        // (issue #324, `crates/roost-engine/src/single_instance.rs`).
        _ = roost_flock(lockFD, LOCK_UN)
        // We do NOT unlink the lockfile —
        // unlinking on shutdown would race with a concurrent second
        // launch that already opened the same path; the GTK side
        // uses the same "leave it on disk" convention. The PID in
        // the file is the only thing that lies, and the lock
        // status (acquired vs. blocked) is the source of truth.
        Darwin.close(lockFD)
    }

    /// Try to acquire the single-instance lock at `lockPath`.
    /// `ROOST_ALLOW_MULTI=1` short-circuits to `.bypassed` — useful
    /// when running `swift test`, Xcode debug builds, or
    /// intentional multi-instance experimentation.
    static func acquire(
        lockPath: String,
        environment: [String: String] = ProcessInfo.processInfo.environment
    ) throws -> Status {
        if let multi = environment["ROOST_ALLOW_MULTI"], multi == "1" {
            return .bypassed
        }

        let parent = (lockPath as NSString).deletingLastPathComponent
        try? FileManager.default.createDirectory(
            atPath: parent,
            withIntermediateDirectories: true
        )

        // O_CLOEXEC so the lock fd doesn't survive across an
        // exec(3) — the child should re-acquire its own lock if it
        // really wants to enforce single-instance.
        let fd = open(lockPath, O_CREAT | O_RDWR | O_CLOEXEC, 0o600)
        if fd < 0 {
            throw SingleInstanceError.openFailed(path: lockPath, errno: errno)
        }

        let rc = roost_flock(fd, LOCK_EX | LOCK_NB)
        if rc < 0 {
            let lockErrno = errno
            let holderPID = readHolderPID(fd: fd)
            Darwin.close(fd)
            if lockErrno == EWOULDBLOCK || lockErrno == EAGAIN {
                return .alreadyHeld(holderPID: holderPID ?? 0)
            }
            throw SingleInstanceError.lockFailed(errno: lockErrno)
        }

        // We own the lock. Truncate the file + rewrite our PID so a
        // subsequent contender can read the new holder. ftruncate
        // is async-signal-safe and the lock guarantees no
        // concurrent reader.
        ftruncate(fd, 0)
        lseek(fd, 0, SEEK_SET)
        let pidLine = "\(getpid())\n"
        let written = pidLine.withCString { cstr -> ssize_t in
            Darwin.write(fd, cstr, strlen(cstr))
        }
        if written < 0 {
            let writeErrno = errno
            // No `SingleInstance` exists yet, so `deinit`'s LOCK_UN can't
            // run — release here or a forked child could hold the lock on
            // past this close (#324).
            _ = roost_flock(fd, LOCK_UN)
            Darwin.close(fd)
            throw SingleInstanceError.writeFailed(errno: writeErrno)
        }

        return .acquired(SingleInstance(lockFD: fd, lockPath: lockPath))
    }

    /// Outcome of an `acquirePair(...)` attempt.
    enum PairStatus {
        /// Start. The locks live for the process's lifetime.
        case acquired(InstanceLocks)
        /// A live instance owns the socket: activate it and exit 0.
        case alreadyRunning(holderPID: pid_t)
        /// We took the socket lock — so nothing is listening on our
        /// socket — and another process owns our state directory. By
        /// construction that process is on a different socket
        /// directory, so there is nothing to activate: refuse to start.
        case stateHeld(holderPID: pid_t, statePath: String)
        /// `ROOST_ALLOW_MULTI=1`; neither lock was taken.
        case bypassed
    }

    /// Take both single-instance locks: socket/bind first, then state.
    ///
    /// Failure policy, per lock — deliberately not uniform:
    ///
    /// * **State lock**: fail closed. Contention returns `.stateHeld`;
    ///   any other failure throws `.stateLockUnavailable`. `state.json`
    ///   has no second line of defence, and a process holding the
    ///   socket lock but not the state lock would happily write it.
    /// * **Socket lock**: contention returns `.alreadyRunning`; any
    ///   other failure is reported on `InstanceLocks.socketLockError`
    ///   and the app starts degraded. That is the pre-existing Mac
    ///   policy and it stays, because the socket has a second guard the
    ///   state file doesn't: without the lock, `IPCServer` refuses on
    ///   `EADDRINUSE` instead of unlinking. Rust has no such bypass
    ///   path and treats both as fatal.
    ///
    /// Release is the reverse of acquisition; `InstanceLocks` owns that
    /// ordering, and this function releases the socket lock explicitly
    /// before returning `.stateHeld` so a refusing launch never sits on
    /// a lock a peer is waiting for.
    static func acquirePair(
        socketLockPath: String,
        stateLockPath: String,
        environment: [String: String] = ProcessInfo.processInfo.environment
    ) throws -> PairStatus {
        if let multi = environment["ROOST_ALLOW_MULTI"], multi == "1" {
            return .bypassed
        }

        var socket: SingleInstance?
        var socketError: (any Error)?
        do {
            switch try takeLock(socketLockPath) {
            case .taken(let lock): socket = lock
            case .held(let pid): return .alreadyRunning(holderPID: pid)
            }
        } catch {
            socketError = error
        }

        // Defence in depth for the case where both paths name one file.
        // `BundleProfile` gives them different filenames, so getting
        // here takes a caller passing one path twice, or a state dir
        // symlinked onto the socket dir with matching names. `flock`
        // belongs to the open file description, so a second open+flock
        // of the same file returns EWOULDBLOCK and we would refuse to
        // start against ourselves. Hold the one lock instead.
        if socket != nil, sameFile(socketLockPath, stateLockPath) {
            return .acquired(
                InstanceLocks(socket: socket, state: nil, socketLockError: socketError))
        }

        let state: SingleInstance
        do {
            switch try takeLock(stateLockPath) {
            case .taken(let lock):
                state = lock
            case .held(let pid):
                socket = nil  // explicit reverse release before we return
                return .stateHeld(holderPID: pid, statePath: stateLockPath)
            }
        } catch {
            socket = nil
            throw SingleInstanceError.stateLockUnavailable(
                path: stateLockPath, underlying: error)
        }

        return .acquired(
            InstanceLocks(socket: socket, state: state, socketLockError: socketError))
    }

    private enum LockOutcome {
        case taken(SingleInstance)
        case held(pid_t)
    }

    /// `acquire` minus the `.bypassed` case: the environment is emptied
    /// so the `ROOST_ALLOW_MULTI` check inside it can't fire (the
    /// caller already handled that env var once, for both locks).
    private static func takeLock(_ path: String) throws -> LockOutcome {
        switch try acquire(lockPath: path, environment: [:]) {
        case .acquired(let lock): return .taken(lock)
        case .alreadyHeld(let pid): return .held(pid)
        case .bypassed: throw SingleInstanceError.lockFailed(errno: EINVAL)
        }
    }

    /// Do these two paths name the same file? Compares parents
    /// resolved through `realpath(3)` — the lock files themselves may
    /// not exist yet, and a symlinked state dir must still be caught.
    /// Mirrors `same_file` in `single_instance.rs`.
    private static func sameFile(_ a: String, _ b: String) -> Bool {
        if a == b { return true }
        return withCanonicalParent(a) == withCanonicalParent(b)
    }

    private static func withCanonicalParent(_ path: String) -> String {
        let ns = path as NSString
        let parent = ns.deletingLastPathComponent
        let name = ns.lastPathComponent
        guard !parent.isEmpty, !name.isEmpty else { return path }
        guard let resolved = realpath(parent, nil) else { return path }
        defer { free(resolved) }
        return (String(cString: resolved) as NSString).appendingPathComponent(name)
    }

    /// Read the PID embedded in the lockfile at the given fd. Best-
    /// effort — returns nil if the file is empty (a contender that
    /// raced past our truncate but before our write) or if the
    /// content doesn't parse as a number.
    private static func readHolderPID(fd: Int32) -> pid_t? {
        lseek(fd, 0, SEEK_SET)
        var buf = [UInt8](repeating: 0, count: 32)
        let n = buf.withUnsafeMutableBufferPointer { ptr -> ssize_t in
            Darwin.read(fd, ptr.baseAddress, ptr.count)
        }
        guard n > 0 else { return nil }
        let text = String(decoding: buf.prefix(Int(n)), as: UTF8.self)
            .trimmingCharacters(in: .whitespacesAndNewlines)
        return pid_t(text)
    }
}

/// The locks a running Mac UI holds, released in the reverse of the
/// order they were taken.
///
/// Swift does not guarantee the deinit order of stored properties, and
/// the release order matters (state, then socket — see
/// `SingleInstance.acquirePair`), so `deinit` does it by hand instead
/// of leaving it to ARC's discretion.
final class InstanceLocks {
    private var state: SingleInstance?
    private var socket: SingleInstance?

    /// Non-nil when the socket lock could not be taken for a reason
    /// other than contention. The app runs degraded: `IPCServer` is
    /// told it does not hold the lock, so it refuses on `EADDRINUSE`
    /// rather than recovering a socket it can't prove is stale.
    let socketLockError: (any Error)?

    /// False under the degraded case above. Gates M6's stale-socket
    /// recovery, which is only sound while we own the socket lock.
    var holdsSocketLock: Bool { socket != nil }

    var socketLockPath: String? { socket?.lockPath }
    /// Nil when both paths named one file and the single held lock
    /// guards both.
    var stateLockPath: String? { state?.lockPath }

    init(socket: SingleInstance?, state: SingleInstance?, socketLockError: (any Error)?) {
        self.socket = socket
        self.state = state
        self.socketLockError = socketLockError
    }

    /// Release both, state first. Idempotent.
    func release() {
        state = nil
        socket = nil
    }

    deinit {
        state = nil
        socket = nil
    }
}

private func strerrorString(_ code: Int32) -> String {
    if let c = strerror(code), let s = String(validatingCString: c) {
        return s
    }
    return "errno \(code)"
}
