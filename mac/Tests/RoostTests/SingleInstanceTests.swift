// SingleInstanceTests — covers the M4c flock-based single-instance
// enforcement. The Rust side has its own coverage in
// `crates/roost-engine/src/single_instance.rs`'s (shared by iced)
// embedded #[cfg(test)] module; this file is the Swift mirror.

import Darwin
import Foundation
import Testing

@testable import Roost

@Suite("SingleInstance flock guard")
struct SingleInstanceTests {
    @Test func firstAcquireReturnsAcquired() throws {
        let path = uniqueLockPath()
        defer { unlink(path) }

        switch try SingleInstance.acquire(lockPath: path) {
        case .acquired(let inst):
            #expect(inst.lockPath == path)
        case .alreadyHeld(let pid):
            Issue.record("expected acquired, got alreadyHeld(\(pid))")
        case .bypassed:
            Issue.record("expected acquired, got bypassed")
        }
    }

    @Test func secondAcquireSeesAlreadyHeld() throws {
        let path = uniqueLockPath()
        defer { unlink(path) }

        let first = try SingleInstance.acquire(lockPath: path)
        guard case .acquired = first else {
            Issue.record("first acquire failed: \(first)")
            return
        }
        // The flock is held by THIS process for the lifetime of
        // `first`. flock is per-fd, so a same-process second
        // acquire on a different fd still contends — that's the
        // observable behavior we want.
        switch try SingleInstance.acquire(lockPath: path) {
        case .acquired:
            Issue.record("second acquire unexpectedly succeeded")
        case .alreadyHeld(let pid):
            // Holder PID should match our own process PID since
            // `first` wrote it.
            #expect(pid == getpid())
        case .bypassed:
            Issue.record("second acquire returned bypassed without env var")
        }
        _ = first  // keep `first` alive past the second acquire
    }

    @Test func envVarBypassReturnsBypassed() throws {
        let path = uniqueLockPath()
        defer { unlink(path) }

        let status = try SingleInstance.acquire(
            lockPath: path,
            environment: ["ROOST_ALLOW_MULTI": "1"]
        )
        guard case .bypassed = status else {
            Issue.record("expected bypassed, got \(status)")
            return
        }
        // Bypass should NOT create the lockfile — otherwise the
        // bypass would leave inert state on disk.
        #expect(!FileManager.default.fileExists(atPath: path))
    }

    @Test func releaseOnDeinitAllowsReAcquire() throws {
        let path = uniqueLockPath()
        defer { unlink(path) }

        do {
            let first = try SingleInstance.acquire(lockPath: path)
            guard case .acquired = first else {
                Issue.record("first acquire failed: \(first)")
                return
            }
            // `first` goes out of scope at the end of this block —
            // its deinit closes the fd and releases the flock.
        }

        // A fresh acquire on the same path must now succeed.
        switch try SingleInstance.acquire(lockPath: path) {
        case .acquired: break  // expected
        case .alreadyHeld(let pid):
            Issue.record("expected re-acquire, got alreadyHeld(\(pid))")
        case .bypassed:
            Issue.record("expected re-acquire, got bypassed")
        }
    }

    // Regression guard for #324, mirroring the Rust
    // `drop_releases_even_when_a_forked_child_inherited_the_fd`.
    // flock(2) locks belong to the open file description, not the fd
    // or the process, so a fork()ed child that inherited the fd keeps
    // the lock alive past our close(2). The app forks on every PTY
    // spawn. Clearing FD_CLOEXEC makes the fork→exec window
    // deterministic instead of intermittent.
    //
    // Foundation's `Process` is deliberately NOT used here: on Darwin it
    // spawns with POSIX_SPAWN_CLOEXEC_DEFAULT, which closes every fd in
    // the child regardless of FD_CLOEXEC, so the test would pass
    // vacuously. Plain `posix_spawn` inherits normally.
    @Test func releaseOnDeinitSurvivesAForkedChildHoldingTheFD() throws {
        let path = uniqueLockPath()
        defer { unlink(path) }

        var child: pid_t = 0
        defer {
            if child > 0 {
                kill(child, SIGKILL)
                var status: Int32 = 0
                waitpid(child, &status, 0)
            }
        }

        do {
            let first = try SingleInstance.acquire(lockPath: path)
            guard case .acquired(let inst) = first else {
                Issue.record("first acquire failed: \(first)")
                return
            }
            #expect(fcntl(inst.lockFD, F_SETFD, 0) == 0)

            var argv: [UnsafeMutablePointer<CChar>?] = [strdup("/bin/sleep"), strdup("30"), nil]
            defer { argv.forEach { free($0) } }
            #expect(posix_spawn(&child, "/bin/sleep", nil, nil, &argv, environ) == 0)
        }

        switch try SingleInstance.acquire(lockPath: path) {
        case .acquired: break
        case .alreadyHeld(let pid):
            Issue.record("an inherited fd blocked re-acquire: alreadyHeld(\(pid))")
        case .bypassed:
            Issue.record("expected re-acquire, got bypassed")
        }
    }

    private func uniqueLockPath() -> String {
        let id = UUID().uuidString
        return "/tmp/roost-tests-\(id).lock"
    }
}

/// The two-lock composition. Mirrors the `acquire_locks` tests in
/// `crates/roost-engine/src/single_instance.rs`.
@Suite("SingleInstance two-lock acquisition")
struct InstanceLocksTests {
    /// The bug the state lock exists to fix: same state dir, different
    /// runtime dirs. Both processes used to start and write one
    /// `state.json`.
    @Test func sameStateDirWithDifferentRuntimeDirsContends() throws {
        let runtimeA = try uniqueDir()
        let runtimeB = try uniqueDir()
        let state = try uniqueDir()
        defer { removeAll([runtimeA, runtimeB, state]) }

        let first = try SingleInstance.acquirePair(
            socketLockPath: runtimeA + "/roost.lock",
            stateLockPath: state + "/state.lock",
            environment: [:]
        )
        guard case .acquired(let held) = first else {
            Issue.record("first acquire failed: \(first)")
            return
        }

        let second = try SingleInstance.acquirePair(
            socketLockPath: runtimeB + "/roost.lock",
            stateLockPath: state + "/state.lock",
            environment: [:]
        )
        switch second {
        case .stateHeld(let pid, let path):
            #expect(pid == getpid())
            #expect(path == state + "/state.lock")
        default:
            Issue.record("expected stateHeld, got \(second)")
        }
        _ = held
    }

    /// The regression a naive "move the one lock next to state.json"
    /// would have introduced: different state dirs, one socket.
    @Test func sameSocketWithDifferentStateDirsContends() throws {
        let runtime = try uniqueDir()
        let stateA = try uniqueDir()
        let stateB = try uniqueDir()
        defer { removeAll([runtime, stateA, stateB]) }

        let first = try SingleInstance.acquirePair(
            socketLockPath: runtime + "/roost.lock",
            stateLockPath: stateA + "/state.lock",
            environment: [:]
        )
        guard case .acquired(let held) = first else {
            Issue.record("first acquire failed: \(first)")
            return
        }

        let second = try SingleInstance.acquirePair(
            socketLockPath: runtime + "/roost.lock",
            stateLockPath: stateB + "/state.lock",
            environment: [:]
        )
        switch second {
        case .alreadyRunning(let pid): #expect(pid == getpid())
        default: Issue.record("expected alreadyRunning, got \(second)")
        }
        _ = held
    }

    /// Reverse release: a launch that refuses because the state dir is
    /// taken must not sit on the socket lock.
    @Test func refusingOnTheStateLockReleasesTheSocketLock() throws {
        let runtime = try uniqueDir()
        let state = try uniqueDir()
        defer { removeAll([runtime, state]) }
        let socketLock = runtime + "/roost.lock"
        let stateLock = state + "/state.lock"

        let holder = try SingleInstance.acquire(lockPath: stateLock, environment: [:])
        guard case .acquired = holder else {
            Issue.record("could not hold the state lock: \(holder)")
            return
        }

        let refused = try SingleInstance.acquirePair(
            socketLockPath: socketLock, stateLockPath: stateLock, environment: [:])
        guard case .stateHeld = refused else {
            Issue.record("expected stateHeld, got \(refused)")
            return
        }

        switch try SingleInstance.acquire(lockPath: socketLock, environment: [:]) {
        case .acquired: break  // the socket lock was released
        default: Issue.record("the socket lock was not released on refusal")
        }
        _ = holder
    }

    /// R1. When both paths name one file — the HOME-less
    /// `/tmp/<appLabel>` profile is the real-world way there — a second
    /// `flock` would contend with the first, because locks belong to
    /// the open file description. Degrade to one acquisition rather
    /// than refuse to start against ourselves.
    @Test func oneSharedPathDegradesToASingleAcquisition() throws {
        let dir = try uniqueDir()
        defer { removeAll([dir]) }
        let path = dir + "/roost.lock"

        let status = try SingleInstance.acquirePair(
            socketLockPath: path, stateLockPath: path, environment: [:])
        guard case .acquired(let locks) = status else {
            Issue.record("a shared path must not contend with itself: \(status)")
            return
        }
        #expect(locks.holdsSocketLock)
        #expect(locks.stateLockPath == nil)
    }

    /// The same degradation reached through a symlinked state dir,
    /// which is how the paths can differ textually but name one file.
    @Test func aSymlinkedStateDirDegradesToASingleAcquisition() throws {
        let dir = try uniqueDir()
        defer { removeAll([dir]) }
        let runtime = dir + "/runtime"
        let linked = dir + "/state-link"
        try FileManager.default.createDirectory(
            atPath: runtime, withIntermediateDirectories: true)
        try FileManager.default.createSymbolicLink(atPath: linked, withDestinationPath: runtime)

        let status = try SingleInstance.acquirePair(
            socketLockPath: runtime + "/roost.lock",
            stateLockPath: linked + "/roost.lock",
            environment: [:]
        )
        guard case .acquired(let locks) = status else {
            Issue.record("a symlinked state dir must not contend with itself: \(status)")
            return
        }
        #expect(locks.stateLockPath == nil)
    }

    /// Distinct directories are the normal case and must NOT degrade,
    /// or the same-path guard would silently disable the state lock.
    @Test func distinctPathsTakeBothLocks() throws {
        let runtime = try uniqueDir()
        let state = try uniqueDir()
        defer { removeAll([runtime, state]) }

        let status = try SingleInstance.acquirePair(
            socketLockPath: runtime + "/roost.lock",
            stateLockPath: state + "/state.lock",
            environment: [:]
        )
        guard case .acquired(let locks) = status else {
            Issue.record("expected both locks, got \(status)")
            return
        }
        #expect(locks.holdsSocketLock)
        #expect(locks.stateLockPath == state + "/state.lock")
        #expect(locks.socketLockError == nil)
    }

    /// D5.7 — fail closed. A state lock that cannot be opened at all
    /// (its parent is a regular file) throws, and App.swift's `catch`
    /// terminates. Rust is fatal here too. The alternative — start
    /// anyway — is a process holding the socket lock, no state lock,
    /// and a `state.json` it will happily write.
    @Test func aStateLockThatCannotBeOpenedFailsClosed() throws {
        let dir = try uniqueDir()
        defer { removeAll([dir]) }
        let runtime = dir + "/runtime"
        try FileManager.default.createDirectory(
            atPath: runtime, withIntermediateDirectories: true)
        // A regular file where the state dir should be: create_dir and
        // open both fail underneath it.
        let blocker = dir + "/blocker"
        try Data("x".utf8).write(to: URL(fileURLWithPath: blocker))

        #expect(throws: SingleInstance.SingleInstanceError.self) {
            _ = try SingleInstance.acquirePair(
                socketLockPath: runtime + "/roost.lock",
                stateLockPath: blocker + "/state.lock",
                environment: [:]
            )
        }

        // ...and the socket lock it took on the way is released.
        switch try SingleInstance.acquire(lockPath: runtime + "/roost.lock", environment: [:]) {
        case .acquired: break
        default: Issue.record("the socket lock was not released before failing closed")
        }
    }

    @Test func multiBypassTakesNeitherLock() throws {
        let runtime = try uniqueDir()
        let state = try uniqueDir()
        defer { removeAll([runtime, state]) }

        let status = try SingleInstance.acquirePair(
            socketLockPath: runtime + "/roost.lock",
            stateLockPath: state + "/state.lock",
            environment: ["ROOST_ALLOW_MULTI": "1"]
        )
        guard case .bypassed = status else {
            Issue.record("expected bypassed, got \(status)")
            return
        }
        #expect(!FileManager.default.fileExists(atPath: runtime + "/roost.lock"))
        #expect(!FileManager.default.fileExists(atPath: state + "/state.lock"))
    }

    @Test func releaseFreesBothForTheNextStart() throws {
        let runtime = try uniqueDir()
        let state = try uniqueDir()
        defer { removeAll([runtime, state]) }
        let socketLock = runtime + "/roost.lock"
        let stateLock = state + "/state.lock"

        let first = try SingleInstance.acquirePair(
            socketLockPath: socketLock, stateLockPath: stateLock, environment: [:])
        guard case .acquired(let locks) = first else {
            Issue.record("first acquire failed: \(first)")
            return
        }
        locks.release()

        let second = try SingleInstance.acquirePair(
            socketLockPath: socketLock, stateLockPath: stateLock, environment: [:])
        guard case .acquired = second else {
            Issue.record("both locks must be free after release: \(second)")
            return
        }
    }

    private func uniqueDir() throws -> String {
        let path = "/tmp/roost-tests-\(UUID().uuidString)"
        try FileManager.default.createDirectory(atPath: path, withIntermediateDirectories: true)
        return path
    }

    private func removeAll(_ paths: [String]) {
        for path in paths {
            try? FileManager.default.removeItem(atPath: path)
        }
    }
}
