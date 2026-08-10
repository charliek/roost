// SingleInstanceTests — covers the M4c flock-based single-instance
// enforcement. The Rust side has its own coverage in
// `crates/roost-linux/src/single_instance.rs`'s embedded #[cfg(test)]
// module; this file is the Swift mirror.

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
