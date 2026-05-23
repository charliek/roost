// SingleInstance.swift — daemon-removal refactor M4c.
//
// Mac-side single-instance enforcement. Mirrors the GTK side in
// `crates/roost-linux/src/single_instance.rs` so the two variants
// have the same observable behavior: first launch acquires an
// exclusive `flock(LOCK_EX | LOCK_NB)` on `<socket-dir>/roost.lock`
// and writes its PID; second launch fails the lock, reads the PID
// from the existing file, and exits 0. The existing window is
// activated by the IPC layer (M6 hardens that path); this file
// only owns lock acquisition + PID writing.
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

        var description: String {
            switch self {
            case .openFailed(let p, let e): return "open(\(p)) failed: \(strerrorString(e))"
            case .lockFailed(let e): return "flock failed: \(strerrorString(e))"
            case .writeFailed(let e): return "write(pid) failed: \(strerrorString(e))"
            }
        }
    }

    private let lockFD: Int32
    let lockPath: String

    private init(lockFD: Int32, lockPath: String) {
        self.lockFD = lockFD
        self.lockPath = lockPath
    }

    deinit {
        // Closing the fd releases the flock automatically (BSD
        // flock semantics). We do NOT unlink the lockfile —
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
            Darwin.close(fd)
            throw SingleInstanceError.writeFailed(errno: writeErrno)
        }

        return .acquired(SingleInstance(lockFD: fd, lockPath: lockPath))
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

private func strerrorString(_ code: Int32) -> String {
    if let c = strerror(code), let s = String(validatingCString: c) {
        return s
    }
    return "errno \(code)"
}
